// chunk.rs
use std::sync::atomic::AtomicU64;

use crate::types::*;
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::RequestBuilder;
use tokio::sync::broadcast;

/// 用于控制 `run` 方法中主循环的枚举
enum LoopControl {
    /// 继续循环
    Continue,
    /// 正常中断循环 (例如，任务完成或被终止)
    Break,
    /// 因错误而中断循环
    BreakWithError,
}

// 为了在广播通道中使用，我们需要为 ThreadEvent 实现 Clone
// 注意：WriteFile 变体中的 `Bytes` 是浅拷贝，开销很小。
/// 分片上下文结构体
///
/// 包含了一个下载分片所需的所有状态和通信通道。
pub(crate) struct ChunkContext {
    /// 分片的唯一标识符
    pub id: ChunkId,
    /// 线程事件发送通道，用于将数据块等命令发送给文件写入任务
    pub res_tx: std::sync::mpsc::Sender<EventResource>,
    /// 广播接收通道，用于接收全局命令，如“分裂下载”或“全部终止”
    pub evnet_rx: broadcast::Receiver<EventThread>,
    /// 状态信息发送通道，用于广播此分片的下载进度、完成、失败或分裂等状态
    pub state_tx: broadcast::Sender<EventStatus>,
    /// 此分片在整个文件中的起始字节位置
    pub start_byte: u64,
    /// 此分片在整个文件中的结束字节位置
    pub end_byte: u64,
}

impl ChunkContext {
    /// 资源id计数器
    const ID: AtomicU64 = AtomicU64::new(0);

    /// 创建一个新的 ChunkContext 实例
    ///
    /// # Arguments
    /// * `res_tx` - 线程事件发送通道
    /// * `evnet_rx` - 广播接收通道
    /// * `state_tx` - 状态信息发送通道
    /// * `start_byte` - 分片起始字节
    /// * `end_byte` - 分片结束字节
    pub fn new(
        res_tx: std::sync::mpsc::Sender<EventResource>,
        evnet_rx: broadcast::Receiver<EventThread>,
        state_tx: broadcast::Sender<EventStatus>,
        start_byte: u64,
        end_byte: u64,
    ) -> Self {
        let id = Self::ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            id,
            res_tx,
            evnet_rx,
            state_tx,
            start_byte,
            end_byte,
        }
    }

    /// 处理从广播通道接收到的事件
    ///
    /// # Arguments
    /// * `offset` - 当前的下载偏移量
    /// * `end` - 一个可变引用，指向当前分片的结束字节，可能会被此函数修改
    ///
    /// # Returns
    /// * `LoopControl` - 指示主循环应如何继续
    async fn handle_broadcast_event(&mut self, offset: u64) -> LoopControl {
        let event = match self.evnet_rx.recv().await {
            Ok(event) => event,
            // 如果通道关闭，则可以认为任务应该终止
            Err(_) => return LoopControl::Break,
        };

        match event {
            // 如果是“分裂下载”命令，并且 ID 匹配当前分片
            EventThread::SplitChunk { id, strategy } if id == self.id => {
                // 调用策略闭包来计算分割点
                if let Some(new_chunk_start) = strategy(offset, self.end_byte) {
                    // 确保分割点是有效的
                    if new_chunk_start > offset && new_chunk_start <= self.end_byte {
                        let midpoint = new_chunk_start - 1;

                        // 生命周期事件：广播分片已被分裂
                        if self
                            .state_tx
                            .send(EventStatus::ChunkSplited {
                                original_id: self.id,
                                new_start_byte: new_chunk_start,
                                new_end_byte: self.end_byte,
                            })
                            .is_ok()
                        {
                            // 更新当前分片的结束位置
                            self.end_byte = midpoint;
                        }
                    }
                }
            }
            // 如果是“终止分片”命令，并且 ID 匹配当前分片
            EventThread::Terminate(id) if id == self.id => {
                // 生命周期事件：广播分片已被终止
                let _ = self.state_tx.send(EventStatus::ChunkTerminated {
                    id: self.id,
                    start_byte: self.start_byte,
                    end_byte: self.end_byte,
                    downloaded_bytes: offset - self.start_byte,
                });
                return LoopControl::Break;
            }
            // 如果是“全部终止”命令，则中断循环
            EventThread::TerminateAll => return LoopControl::Break,
            // 忽略其他命令
            _ => {}
        }
        LoopControl::Continue
    }

    /// 处理从网络流中读取的数据块
    ///
    /// # Arguments
    /// * `chunk_result` - 从 `bytes_stream` 中获取的结果
    /// * `offset` - 一个可变引用，指向当前的下载偏移量，会被此函数修改
    /// * `end` - 当前分片的结束字节
    ///
    /// # Returns
    /// * `LoopControl` - 指示主循环应如何继续
    fn handle_stream_chunk(
        &self,
        chunk_result: Option<std::result::Result<Bytes, reqwest::Error>>,
        offset: &mut u64,
    ) -> LoopControl {
        let chunk = match chunk_result {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                // 生命周期事件：广播因流错误导致的分片失败
                let _ = self.state_tx.send(EventStatus::ChunkFailed {
                    id: self.id,
                    start_byte: *offset,
                    end_byte: self.end_byte,
                    downloaded_bytes: *offset - self.start_byte,
                    error: e.to_string().into(),
                });
                return LoopControl::BreakWithError;
            }
            None => return LoopControl::Break, // 流正常结束
        };

        // 如果当前偏移量已经超过了此分片的目标，则停止
        if *offset > self.end_byte {
            return LoopControl::Break;
        }

        // 计算此分片还需下载的字节数
        let remaining_bytes = self.end_byte - *offset + 1;
        let chunk_len = chunk.len() as u64;

        // 实际写入的长度不能超过剩余需要下载的长度
        let write_len = std::cmp::min(remaining_bytes, chunk_len);

        // 如果 write_len 为 0，说明已完成，可以退出
        if write_len == 0 {
            return LoopControl::Break;
        }

        // 优化点 1: 避免不必要的克隆，优先移动所有权
        let to_write = if write_len < chunk_len {
            chunk.slice(..write_len as usize)
        } else {
            chunk // 直接移动，零开销
        };

        // 发送写入文件的线程事件
        if self
            .res_tx
            .send(EventResource::WriteFile {
                offset: *offset,
                data: to_write,
            })
            .is_err()
        {
            // 生命周期事件：广播因写入通道关闭导致的分片失败
            let _ = self.state_tx.send(EventStatus::ChunkFailed {
                id: self.id,
                start_byte: *offset,
                end_byte: self.end_byte,
                downloaded_bytes: *offset - self.start_byte,
                error: "写入文件通道已关闭".into(),
            });
            return LoopControl::BreakWithError;
        }

        // 更新偏移量
        *offset += write_len;

        // 生命周期事件：广播下载进度
        let _ = self.state_tx.send(EventStatus::ChunkProgress {
            id: self.id,
            start_byte: self.start_byte,
            end_byte: self.end_byte,
            downloaded_bytes: *offset - self.start_byte,
        });

        // 如果实际写入的字节数小于接收到的数据块长度，
        // 说明我们已经到达了这个分片的末尾，不需要再处理流中的任何数据了。
        if write_len < chunk_len {
            return LoopControl::Break;
        }

        LoopControl::Continue
    }

    /// 运行分片下载任务
    ///
    /// 此方法负责发送 HTTP 请求，接收响应流，并根据广播事件处理数据。
    /// 它会持续运行，直到分片完成、失败或收到终止信号。
    pub(crate) async fn run(mut self, req_builder: RequestBuilder) {
        let mut offset = self.start_byte;

        let _ = self.state_tx.send(EventStatus::ChunkStarted {
            id: self.id,
            start_byte: self.start_byte,
            end_byte: self.end_byte,
        });

        let range_header = format!("bytes={}-{}", self.start_byte, self.end_byte);
        let response = match req_builder
            .try_clone()
            .expect("Request builder should be cloneable before sending")
            .header("Range", range_header)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => resp,
            Err(e) => {
                let _ = self.state_tx.send(EventStatus::ChunkFailed {
                    id: self.id,
                    start_byte: self.start_byte,
                    end_byte: self.end_byte,
                    downloaded_bytes: 0,
                    error: e.to_string().into(),
                });
                return;
            }
        };

        let mut stream = response.bytes_stream();
        let mut failed = false;

        loop {
            let control = tokio::select! {
                biased;

                control = self.handle_broadcast_event(offset) => control,
                chunk_result = stream.next() => self.handle_stream_chunk(chunk_result, &mut offset),
            };

            match control {
                LoopControl::Continue => continue,
                LoopControl::Break => break,
                LoopControl::BreakWithError => {
                    failed = true;
                    break;
                }
            }
        }

        if !failed {
            let _ = self
                .state_tx
                .send(EventStatus::ChunkCompleted { id: self.id });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    // 测试：收到 SplitChunk 事件且策略返回有效分割点时，应广播 ChunkSplited 并更新 end
    #[tokio::test]
    async fn test_split() {
        let (tx, _rx) = broadcast::channel(10);
        let (res_tx, _res_rx) = std::sync::mpsc::channel();
        let (state_tx, mut state_rx) = broadcast::channel(10);

        // 策略：offset==100 时建议从 150 分割
        let strategy: Arc<dyn Fn(u64, u64) -> Option<u64> + Send + Sync> =
            Arc::new(|o, _| (o == 100).then_some(150));

        let mut ctx = ChunkContext {
            id: 1,
            res_tx,
            evnet_rx: tx.subscribe(),
            state_tx: state_tx.clone(),
            start_byte: 100,
            end_byte: 200,
        };

        // 发送分割命令
        tx.send(EventThread::SplitChunk { id: 1, strategy })
            .unwrap();
        let ctrl = ctx.handle_broadcast_event(100).await;
        assert!(matches!(ctrl, LoopControl::Continue));
        assert_eq!(ctx.end_byte, 149); // 原分片被截断到 149

        // 验证广播内容
        match state_rx.recv().await.unwrap() {
            EventStatus::ChunkSplited {
                original_id,
                new_start_byte,
                new_end_byte,
            } => {
                assert_eq!(original_id, 1);
                assert_eq!(new_start_byte, 150);
                assert_eq!(new_end_byte, 200);
            }
            _ => panic!("unexpected msg"),
        }
    }

    // 测试：收到 Terminate 事件后应广播 ChunkTerminated 并返回 Break
    #[tokio::test]
    async fn test_terminate() {
        let (tx, _rx) = broadcast::channel(10);
        let (res_tx, _res_rx) = std::sync::mpsc::channel();
        let (state_tx, mut state_rx) = broadcast::channel(10);

        let mut ctx = ChunkContext {
            id: 2,
            res_tx,
            evnet_rx: tx.subscribe(),
            state_tx: state_tx.clone(),
            start_byte: 300,
            end_byte: 400,
        };

        tx.send(EventThread::Terminate(2)).unwrap();
        let ctrl = ctx.handle_broadcast_event(350).await;
        assert!(matches!(ctrl, LoopControl::Break));

        match state_rx.recv().await.unwrap() {
            EventStatus::ChunkTerminated {
                id,
                start_byte,
                end_byte,
                downloaded_bytes,
            } => {
                assert_eq!(id, 2);
                assert_eq!(start_byte, 300);
                assert_eq!(end_byte, 400);
                assert_eq!(downloaded_bytes, 50); // 350-300
            }
            _ => panic!("unexpected msg"),
        }
    }

    // 测试：正常写入数据块，应发送 WriteFile 事件并广播进度
    #[tokio::test]
    async fn test_write_chunk() {
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let (state_tx, mut state_rx) = broadcast::channel(10);

        let ctx = ChunkContext {
            id: 3,
            res_tx,
            evnet_rx: broadcast::channel(10).1,
            state_tx: state_tx.clone(),
            start_byte: 500,
            end_byte: 600,
        };

        let data = Bytes::from("hello");
        let mut offset = 500;
        let ctrl = ctx.handle_stream_chunk(Some(Ok(data.clone())), &mut offset);
        assert!(matches!(ctrl, LoopControl::Continue));
        assert_eq!(offset, 505);

        // 检查 WriteFile 事件
        match res_rx.try_recv().unwrap() {
            EventResource::WriteFile { offset: o, data: d } => {
                assert_eq!(o, 500);
                assert_eq!(d, data);
            }
            _ => panic!("unexpected event"),
        }

        // 检查进度广播
        match state_rx.recv().await.unwrap() {
            EventStatus::ChunkProgress {
                id,
                start_byte,
                end_byte,
                downloaded_bytes,
            } => {
                assert_eq!(id, 3);
                assert_eq!(start_byte, 500);
                assert_eq!(end_byte, 600);
                assert_eq!(downloaded_bytes, 5);
            }
            _ => panic!("unexpected msg"),
        }
    }

    // 测试：数据块超出分片末尾，应截断并立即结束
    #[tokio::test]
    async fn test_chunk_exceeds_end() {
        let (res_tx, _res_rx) = std::sync::mpsc::channel();
        let (state_tx, _state_rx) = broadcast::channel(10);

        let ctx = ChunkContext {
            id: 4,
            res_tx,
            evnet_rx: broadcast::channel(10).1,
            state_tx: state_tx.clone(),
            start_byte: 700,
            end_byte: 705,
        };

        let mut offset = 703;
        let ctrl = ctx.handle_stream_chunk(Some(Ok(Bytes::from("extra"))), &mut offset);
        assert!(matches!(ctrl, LoopControl::Break));
        assert_eq!(offset, 706); // 写完剩余 3 字节后指向 706
    }
}
