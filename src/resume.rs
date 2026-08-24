//! Hash-verified breakpoint resume support.
//!
//! The resume layer intentionally persists a fixed segment ledger instead of the
//! runtime chunk topology.  A resumed run trusts only segments whose persisted
//! hash still matches the bytes on disk, then rebuilds the remaining download
//! ranges from the unverified gaps.

use crate::types::{DownloadError, Result};
use bitcode::{Decode, Encode};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub const DEFAULT_SEGMENT_SIZE: u64 = 64 * 1024;
const METADATA_VERSION: u32 = 1;
const RESUME_EXTENSION: &str = "download.bitcode";

#[derive(Debug, Clone, Encode, Decode)]
pub struct SegmentRecord {
    pub start: u64,
    pub end: u64,
    pub hash: Option<u64>,
}

impl SegmentRecord {
    fn len(&self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct ResumeMetadata {
    pub version: u32,
    pub file_size: u64,
    pub segment_size: u64,
    pub segments: Vec<SegmentRecord>,
}

impl ResumeMetadata {
    pub fn new(file_size: u64, segment_size: u64) -> Self {
        let segment_size = segment_size.max(1);
        let mut segments = Vec::new();
        let mut start = 0;
        while start < file_size {
            let end = (start + segment_size - 1).min(file_size - 1);
            segments.push(SegmentRecord {
                start,
                end,
                hash: None,
            });
            start = end + 1;
        }
        Self {
            version: METADATA_VERSION,
            file_size,
            segment_size,
            segments,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)?;
        bitcode::decode(&bytes).map_err(|error| DownloadError::ResumeMetadata(error.to_string()))
    }

    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp_path = path.with_extension(format!("{RESUME_EXTENSION}.tmp"));
        fs::write(&temp_path, bitcode::encode(self))?;
        fs::rename(temp_path, path)?;
        Ok(())
    }

    async fn save_atomic_async(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let temp_path = path.with_extension(format!("{RESUME_EXTENSION}.tmp"));
        tokio::fs::write(&temp_path, bitcode::encode(self)).await?;
        tokio::fs::rename(temp_path, path).await?;
        Ok(())
    }

    pub fn set_segment_hash(&mut self, segment_index: usize, hash: u64) {
        if let Some(segment) = self.segments.get_mut(segment_index) {
            segment.hash = Some(hash);
        }
    }

    pub fn verified_ranges(&self) -> Vec<(u64, u64)> {
        collect_contiguous_ranges(
            self.segments
                .iter()
                .filter(|segment| segment.hash.is_some())
                .map(|segment| (segment.start, segment.end)),
        )
    }

    pub fn remaining_ranges(&self) -> Vec<(u64, u64)> {
        collect_contiguous_ranges(
            self.segments
                .iter()
                .filter(|segment| segment.hash.is_none())
                .map(|segment| (segment.start, segment.end)),
        )
    }

    pub fn completed_bytes(&self) -> u64 {
        self.segments
            .iter()
            .filter(|segment| segment.hash.is_some())
            .map(SegmentRecord::len)
            .sum()
    }

    fn validate_shape(&self, file_size: u64) -> Result<()> {
        if self.version != METADATA_VERSION {
            return Err(DownloadError::ResumeMetadata(format!(
                "unsupported resume metadata version {}",
                self.version
            )));
        }
        if self.file_size != file_size {
            return Err(DownloadError::ResumeMetadata(format!(
                "resume metadata file size {} does not match current file size {file_size}",
                self.file_size
            )));
        }
        if self.segment_size == 0 {
            return Err(DownloadError::ResumeMetadata(
                "resume metadata segment size must not be zero".to_owned(),
            ));
        }
        Ok(())
    }

    fn verify_against_file(&mut self, output_path: &Path) -> Result<()> {
        let mut file = fs::File::open(output_path)?;
        let file_len = file.metadata()?.len();
        let mut buf = Vec::new();
        for segment in &mut self.segments {
            let Some(expected_hash) = segment.hash else {
                continue;
            };
            if file_len <= segment.end {
                segment.hash = None;
                continue;
            }
            buf.resize(segment.len() as usize, 0);
            file.seek(SeekFrom::Start(segment.start))?;
            file.read_exact(&mut buf)?;
            if hash_bytes(&buf) != expected_hash {
                segment.hash = None;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ResumePlan {
    pub metadata_path: PathBuf,
    pub metadata: Option<ResumeMetadata>,
    pub truncate_output: bool,
    pub remaining_ranges: Vec<(u64, u64)>,
    pub completed_bytes: u64,
}

impl ResumePlan {
    pub fn prepare(output_path: &Path, file_size: u64, enabled: bool) -> Result<Self> {
        let metadata_path = metadata_path_for(output_path);
        if !enabled {
            let _ = fs::remove_file(&metadata_path);
            return Ok(Self {
                metadata_path,
                metadata: None,
                truncate_output: true,
                remaining_ranges: full_ranges(file_size),
                completed_bytes: 0,
            });
        }

        if metadata_path.exists() {
            if !output_path.exists() {
                return Err(DownloadError::ResumeTargetMissing(
                    output_path.to_path_buf(),
                ));
            }

            let mut metadata = ResumeMetadata::load(&metadata_path)?;
            metadata.validate_shape(file_size)?;
            metadata.verify_against_file(output_path)?;
            metadata.save_atomic(&metadata_path)?;

            let remaining_ranges = metadata.remaining_ranges();
            let completed_bytes = metadata.completed_bytes();
            return Ok(Self {
                metadata_path,
                metadata: Some(metadata),
                truncate_output: false,
                remaining_ranges,
                completed_bytes,
            });
        }
        let metadata = ResumeMetadata::new(file_size, DEFAULT_SEGMENT_SIZE);
        // 立即落盘，确保 <64KiB 内中断也能恢复；失败不阻断下载，仅记录错误
        if let Err(error) = metadata.save_atomic(&metadata_path) {
            eprintln!("[Resume] 初始元数据落盘失败: {error}");
        }
        Ok(Self {
            metadata_path,
            metadata: Some(metadata),
            truncate_output: true,
            remaining_ranges: full_ranges(file_size),
            completed_bytes: 0,
        })
    }

    /// 异步入口：通过 `spawn_blocking` 将同步文件 I/O 卸载，避免阻塞 Tokio 运行时
    pub async fn prepare_async(
        output_path: PathBuf,
        file_size: u64,
        enabled: bool,
    ) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::prepare(&output_path, file_size, enabled))
            .await
            .unwrap_or_else(|e| {
                Err(DownloadError::ResumeMetadata(format!(
                    "resume prepare panicked: {e}"
                )))
            })
    }

    pub fn into_recorder(self) -> Option<ResumeRecorder> {
        self.metadata
            .map(|metadata| ResumeRecorder::new(self.metadata_path, metadata))
    }
}

pub struct ResumeRecorder {
    metadata_path: PathBuf,
    metadata: ResumeMetadata,
    covered_ranges: Vec<Vec<(u64, u64)>>,
}

impl ResumeRecorder {
    pub fn new(metadata_path: PathBuf, metadata: ResumeMetadata) -> Self {
        let covered_ranges = metadata
            .segments
            .iter()
            .map(|segment| {
                if segment.hash.is_some() {
                    vec![(segment.start, segment.end)]
                } else {
                    Vec::new()
                }
            })
            .collect();
        Self {
            metadata_path,
            metadata,
            covered_ranges,
        }
    }

    pub async fn record_write(&mut self, file: &mut File, offset: u64, len: u64) -> Result<()> {
        if len == 0 || self.metadata.file_size == 0 {
            return Ok(());
        }
        let write_start = offset;
        let write_end = offset.saturating_add(len).saturating_sub(1);
        let mut changed = false;

        for index in self.segment_indexes_for_range(write_start, write_end) {
            if self.metadata.segments[index].hash.is_some() {
                continue;
            }
            let segment = &self.metadata.segments[index];
            let overlap_start = write_start.max(segment.start);
            let overlap_end = write_end.min(segment.end);
            add_covered_range(&mut self.covered_ranges[index], overlap_start, overlap_end);

            if covers_segment(&self.covered_ranges[index], segment.start, segment.end) {
                let bytes = read_segment(file, segment.start, segment.len()).await?;
                self.metadata.segments[index].hash = Some(hash_bytes(&bytes));
                changed = true;
            }
        }

        if changed {
            self.metadata.save_atomic_async(&self.metadata_path).await?;
        }
        Ok(())
    }

    fn segment_indexes_for_range(&self, start: u64, end: u64) -> std::ops::RangeInclusive<usize> {
        let segment_size = self.metadata.segment_size.max(1);
        let first = (start / segment_size) as usize;
        let last = (end / segment_size).min(self.metadata.segments.len().saturating_sub(1) as u64)
            as usize;
        first..=last
    }
}

pub fn metadata_path_for(output_path: impl AsRef<Path>) -> PathBuf {
    let output_path = output_path.as_ref();
    let file_name = output_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    output_path.with_file_name(format!("{file_name}.{RESUME_EXTENSION}"))
}

pub fn hash_bytes(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

fn full_ranges(file_size: u64) -> Vec<(u64, u64)> {
    if file_size == 0 {
        Vec::new()
    } else {
        vec![(0, file_size - 1)]
    }
}

async fn read_segment(file: &mut File, start: u64, len: u64) -> Result<Vec<u8>> {
    let mut buf = vec![0; len as usize];
    file.seek(SeekFrom::Start(start)).await?;
    file.read_exact(&mut buf).await?;
    Ok(buf)
}

fn add_covered_range(ranges: &mut Vec<(u64, u64)>, start: u64, end: u64) {
    ranges.push((start, end));
    ranges.sort_by_key(|(start, _)| *start);

    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in ranges.drain(..) {
        if let Some((_, last_end)) = merged.last_mut()
            && start <= last_end.saturating_add(1)
        {
            *last_end = (*last_end).max(end);
            continue;
        }
        merged.push((start, end));
    }
    *ranges = merged;
}

fn covers_segment(ranges: &[(u64, u64)], start: u64, end: u64) -> bool {
    ranges
        .iter()
        .any(|(covered_start, covered_end)| *covered_start <= start && *covered_end >= end)
}

fn collect_contiguous_ranges(ranges: impl Iterator<Item = (u64, u64)>) -> Vec<(u64, u64)> {
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in ranges {
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= previous_end.saturating_add(1)
        {
            *previous_end = (*previous_end).max(end);
            continue;
        }
        merged.push((start, end));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_reconstructs_remaining_ranges_from_verified_segments() {
        let mut metadata = ResumeMetadata::new(DEFAULT_SEGMENT_SIZE * 3, DEFAULT_SEGMENT_SIZE);
        metadata.set_segment_hash(0, 11);
        metadata.set_segment_hash(2, 33);

        assert_eq!(
            metadata.remaining_ranges(),
            vec![(DEFAULT_SEGMENT_SIZE, DEFAULT_SEGMENT_SIZE * 2 - 1)]
        );
        assert_eq!(metadata.completed_bytes(), DEFAULT_SEGMENT_SIZE * 2);
    }

    #[test]
    fn remaining_ranges_merge_adjacent_unverified_segments() {
        let mut metadata = ResumeMetadata::new(DEFAULT_SEGMENT_SIZE * 3, DEFAULT_SEGMENT_SIZE);
        metadata.set_segment_hash(0, 11);

        assert_eq!(
            metadata.remaining_ranges(),
            vec![(DEFAULT_SEGMENT_SIZE, DEFAULT_SEGMENT_SIZE * 3 - 1)]
        );
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
    }
}
