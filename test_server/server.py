import http.server
import socketserver
import os
import time
import configparser
import re
from threading import Lock, Thread, get_ident
import socket
from watchdog.observers import Observer
from watchdog.events import FileSystemEventHandler

# --- Configuration Parsing and Locks ---
config = configparser.ConfigParser()
config_lock = Lock()

def load_config():
    """Load configuration from config.ini."""
    with config_lock:
        config.read('config.ini', encoding='utf-8')

# Initial load
load_config()

HOST = config.get('server', 'host', fallback='0.0.0.0')
PORT = config.getint('server', 'port', fallback=8000)
SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
CONFIG_FILE_PATH = os.path.join(SCRIPT_DIR, 'config.ini')
DIRECTORY = config.get('server', 'directory', fallback=os.path.join(SCRIPT_DIR, 'files'))

def parse_speed(speed_str):
    """Parse speed string (e.g., '10m', '512k') into bytes/sec."""
    speed_str = speed_str.lower().strip()
    if not speed_str:
        return float('inf')
    if speed_str.endswith('k'):
        return float(speed_str[:-1]) * 1024
    if speed_str.endswith('m'):
        return float(speed_str[:-1]) * 1024 * 1024
    if speed_str.endswith('g'):
        return float(speed_str[:-1]) * 1024 * 1024 * 1024
    return float(speed_str)

TOTAL_MAX_SPEED = parse_speed(config.get('throttling', 'total_max_speed', fallback=''))
PER_THREAD_MAX_SPEED = parse_speed(config.get('throttling', 'per_thread_max_speed', fallback=''))

# --- Global Speed Monitor (Thread-Safe) ---
class SpeedMonitor:
    def __init__(self):
        self.lock = Lock()
        self.last_time = time.time()
        self.bytes_sent_since_last_check = 0
        self.current_speed = 0

    def add_bytes(self, amount):
        with self.lock:
            self.bytes_sent_since_last_check += amount

    def get_speed(self):
        with self.lock:
            now = time.time()
            elapsed = now - self.last_time
            if elapsed > 1.0:  # Update every second
                self.current_speed = self.bytes_sent_since_last_check / elapsed
                self.bytes_sent_since_last_check = 0
                self.last_time = now
        return self.current_speed

def format_speed(speed_bytes_per_sec):
    """Format speed in bytes/sec to a human-readable string."""
    if speed_bytes_per_sec < 1024:
        return f"{speed_bytes_per_sec:.2f} B/s"
    elif speed_bytes_per_sec < 1024 * 1024:
        return f"{speed_bytes_per_sec / 1024:.2f} KB/s"
    elif speed_bytes_per_sec < 1024 * 1024 * 1024:
        return f"{speed_bytes_per_sec / (1024 * 1024):.2f} MB/s"
    else:
        return f"{speed_bytes_per_sec / (1024 * 1024 * 1024):.2f} GB/s"

speed_monitor = SpeedMonitor()

# --- Global Bandwidth Manager (Thread-Safe) ---
class BandwidthManager:
    def __init__(self, limit):
        self._limit = limit
        self.lock = Lock()
        self.last_time = time.time()
        self.bytes_sent = 0

    @property
    def limit(self):
        with self.lock:
            return self._limit

    @limit.setter
    def limit(self, value):
        with self.lock:
            self._limit = value

    def throttle(self, amount):
        current_limit = self.limit
        if current_limit == float('inf'):
            return
        
        with self.lock:
            self.bytes_sent += amount
            now = time.time()
            elapsed = now - self.last_time
            
            if elapsed > 0:
                expected_time = self.bytes_sent / current_limit
                sleep_time = expected_time - elapsed
                if sleep_time > 0:
                    time.sleep(sleep_time)

            if time.time() - self.last_time > 1.0:
                self.bytes_sent = 0
                self.last_time = time.time()

total_bandwidth_manager = BandwidthManager(TOTAL_MAX_SPEED)

# --- Connection and File Download Status Managers (Thread-Safe) ---
ACTIVE_DOWNLOADS = {}
downloads_lock = Lock()

FILE_DOWNLOADS = {}
file_downloads_lock = Lock()

# --- Throttled File Reader ---
class ThrottledFileReader:
    def __init__(self, file_obj, thread_manager, speed_monitor_instance):
        self.file_obj = file_obj
        self.thread_manager = thread_manager
        self.speed_monitor = speed_monitor_instance

    def read(self, size=-1):
        self.thread_manager.limit = PER_THREAD_MAX_SPEED
        
        chunk = self.file_obj.read(size)
        if chunk:
            chunk_len = len(chunk)
            self.speed_monitor.add_bytes(chunk_len)
            self.thread_manager.throttle(chunk_len)
            total_bandwidth_manager.throttle(chunk_len)
        return chunk

    def __getattr__(self, attr):
        return getattr(self.file_obj, attr)

# --- Custom Request Handler ---
class ThrottledHTTPRequestHandler(http.server.SimpleHTTPRequestHandler):
    range_re = re.compile(r'bytes\s*=\s*(\d+)\s*-\s*(\d*)', re.I)

    def __init__(self, *args, **kwargs):
        ThrottledHTTPRequestHandler.directory = DIRECTORY
        super().__init__(*args, **kwargs)

    def send_head(self):
        path = self.translate_path(self.path)
        if os.path.isdir(path):
            return super().send_head()

        try:
            f = open(path, 'rb')
        except OSError:
            self.send_error(404, "File not found")
            return None

        fs = os.fstat(f.fileno())
        total_size = fs.st_size
        content_type = self.guess_type(path)
        
        # --- MODIFIED: File download tracking ---
        with file_downloads_lock:
            # If a new download for a previously completed file starts, reset it.
            if self.path in FILE_DOWNLOADS and FILE_DOWNLOADS[self.path].get("completed", False):
                del FILE_DOWNLOADS[self.path]

            if self.path not in FILE_DOWNLOADS:
                FILE_DOWNLOADS[self.path] = {
                    "start_time": time.time(),
                    "total_size": total_size,
                    "bytes_downloaded": 0,
                    "completed": False,  # Flag to prevent multiple completion messages
                    "lock": Lock()
                }
        
        range_header = self.headers.get('Range')
        if range_header:
            range_match = self.range_re.match(range_header)
            if range_match:
                start_byte, end_byte = range_match.groups()
                start_byte = int(start_byte)
                
                if end_byte:
                    end_byte = int(end_byte)
                else:
                    end_byte = total_size - 1

                if start_byte >= total_size or end_byte >= total_size or start_byte > end_byte:
                    self.send_error(416, 'Requested Range Not Satisfiable')
                    f.close()
                    return None
                
                self.send_response(206, 'Partial Content')
                self.send_header('Content-type', content_type)
                self.send_header('Accept-Ranges', 'bytes')
                content_length = end_byte - start_byte + 1
                self.send_header('Content-Range', f'bytes {start_byte}-{end_byte}/{total_size}')
                self.send_header('Content-Length', str(content_length))
                self.end_headers()
                f.seek(start_byte)
                return f

        self.send_response(200)
        self.send_header('Content-type', content_type)
        self.send_header('Accept-Ranges', 'bytes')
        self.send_header('Content-Length', str(total_size))
        self.end_headers()
        return f

    def copyfile(self, source, outputfile):
        thread_id = get_ident()
        download_info = {
            "client": self.client_address,
            "file": self.path,
            "socket": self.request,
            "start_time": time.time()
        }

        with downloads_lock:
            ACTIVE_DOWNLOADS[thread_id] = download_info

        file_download_info = FILE_DOWNLOADS.get(self.path)

        try:
            thread_bandwidth_manager = BandwidthManager(PER_THREAD_MAX_SPEED)
            throttled_source = ThrottledFileReader(source, thread_bandwidth_manager, speed_monitor)
            chunk_size = 16 * 1024
            
            while True:
                buf = throttled_source.read(chunk_size)
                if not buf:
                    break
                outputfile.write(buf)
                
                if file_download_info:
                    with file_download_info["lock"]:
                        file_download_info["bytes_downloaded"] += len(buf)

        except (BrokenPipeError, ConnectionAbortedError, socket.error):
            # This is expected when a download client closes a partial connection
            pass
        except Exception as e:
            print(f"\nAn error occurred during transfer of '{self.path}': {e}")
        finally:
            with downloads_lock:
                ACTIVE_DOWNLOADS.pop(thread_id, None)
            # print(f"\nConnection from {self.client_address} (Thread {thread_id}) has ended.")

            # --- MODIFIED: Check if the file download is complete ---
            if file_download_info:
                with file_download_info["lock"]:
                    # Check if complete AND not already marked as complete
                    if file_download_info["bytes_downloaded"] >= file_download_info["total_size"] and not file_download_info["completed"]:
                        # Mark as complete immediately to prevent other threads from printing
                        file_download_info["completed"] = True
                        
                        end_time = time.time()
                        duration = end_time - file_download_info["start_time"]
                        total_size_bytes = file_download_info["total_size"]
                        
                        average_speed = total_size_bytes / duration if duration > 0 else float('inf')

                        print(f"\n[Download Complete] File '{self.path}' has finished downloading.")
                        print(f"  - Total Time: {duration:.2f} seconds")
                        print(f"  - Average Speed: {format_speed(average_speed)}")
                        print("> ", end="", flush=True) # Restore console prompt


# --- Speed Monitoring Function ---
def monitor_speed(monitor):
    while True:
        speed = monitor.get_speed()
        print(f"Current Total Speed: {format_speed(speed)}   ", end='\r')
        time.sleep(1)

# --- Configuration Reloading and Monitoring ---
def reload_config():
    """Thread-safely reload configuration and update limiters."""
    global TOTAL_MAX_SPEED, PER_THREAD_MAX_SPEED
    print("\n[CONFIG] Detected config.ini change, reloading...")
    
    with config_lock:
        config.read(CONFIG_FILE_PATH, encoding='utf-8')
        
        TOTAL_MAX_SPEED = parse_speed(config.get('throttling', 'total_max_speed', fallback=''))
        PER_THREAD_MAX_SPEED = parse_speed(config.get('throttling', 'per_thread_max_speed', fallback=''))
        
        total_bandwidth_manager.limit = TOTAL_MAX_SPEED
    
    total_speed_str = config.get('throttling', 'total_max_speed', fallback='Unlimited')
    per_thread_speed_str = config.get('throttling', 'per_thread_max_speed', fallback='Unlimited')
    
    print(f"[CONFIG] Configuration updated.")
    print(f"[CONFIG] New total speed limit: {total_speed_str}")
    print(f"[CONFIG] New per-thread speed limit: {per_thread_speed_str}")
    print("> ", end="", flush=True)

class ConfigurationWatcher(FileSystemEventHandler):
    """Triggers an event when config.ini is modified."""
    def on_modified(self, event):
        if not event.is_directory and event.src_path.endswith('config.ini'):
            reload_config()

# --- Console Command Handling ---
def handle_list(args):
    """Handles the 'list' command."""
    with downloads_lock:
        if not ACTIVE_DOWNLOADS:
            print("No active downloads.")
        else:
            print("-" * 80)
            print(f"{'Thread ID':<15} {'Client':<22} {'File':<30} {'Duration'}")
            print("-" * 80)
            for tid, info in ACTIVE_DOWNLOADS.items():
                duration = time.time() - info['start_time']
                print(f"{tid:<15} {info['client'][0]}:{info['client'][1]:<15} {info['file']:<30} {int(duration)}s")
            print("-" * 80)

def handle_disconnect(args):
    """Handles the 'disconnect' command."""
    if len(args) != 1:
        print("Usage: disconnect <Thread ID>")
        return
    try:
        tid_to_disconnect = int(args[0])
        with downloads_lock:
            if tid_to_disconnect in ACTIVE_DOWNLOADS:
                print(f"Disconnecting thread {tid_to_disconnect}...")
                sock = ACTIVE_DOWNLOADS[tid_to_disconnect]['socket']
                sock.shutdown(socket.SHUT_RDWR)
                sock.close()
                print(f"Disconnection signal sent to thread {tid_to_disconnect}.")
            else:
                print(f"Error: Thread with ID {tid_to_disconnect} not found.")
    except ValueError:
        print("Error: Thread ID must be an integer.")
    except Exception as e:
        print(f"Error while trying to disconnect thread: {e}")

def handle_help(args):
    """Handles the 'help' command."""
    print("\nAvailable commands:")
    for cmd, description in COMMAND_DESCRIPTIONS.items():
        aliases = [alias for alias, full_cmd in ALIASES.items() if full_cmd == cmd]
        alias_str = f"(aliases: {', '.join(aliases)})" if aliases else ""
        print(f"  {cmd:<15} {alias_str:<20} - {description}")
    print()

COMMAND_DESCRIPTIONS = {
    'list': 'List all active download connections.',
    'disconnect': 'Forcibly disconnect a download by its Thread ID (e.g., d 12345).',
    'help': 'Show this help message.',
}

COMMANDS = {
    'list': handle_list,
    'disconnect': handle_disconnect,
    'help': handle_help,
}

ALIASES = {
    'l': 'list', 'ls': 'list',
    'd': 'disconnect', 'kill': 'disconnect',
    'h': 'help', '?': 'help',
}

def console_control():
    """Run the interactive console for server management."""
    print("\nConsole started. Type 'help' or 'h' for available commands.")
    while True:
        try:
            command_line = input("> ").strip()
            if not command_line:
                continue

            parts = command_line.split()
            base_command = parts[0].lower()
            args = parts[1:]

            command_name = ALIASES.get(base_command, base_command)
            handler = COMMANDS.get(command_name)
            
            if handler:
                handler(args)
            else:
                print(f"Unknown command: '{base_command}'. Type 'help' or 'h' for assistance.")

        except (EOFError, KeyboardInterrupt):
            print("\nPlease use Ctrl+C in the main program window to stop the server.")

# --- Server Initialization ---
if __name__ == "__main__":
    if not os.path.exists(DIRECTORY):
        print(f"Warning: Directory '{DIRECTORY}' does not exist. Creating it...")
        os.makedirs(DIRECTORY)
    
    os.chdir(SCRIPT_DIR)
    
    event_handler = ConfigurationWatcher()
    observer = Observer()
    observer.schedule(event_handler, path='.', recursive=False)
    observer.daemon = True
    observer.start()
    print("Monitoring of config.ini has started.")
    
    os.chdir(DIRECTORY)

    Handler = ThrottledHTTPRequestHandler
    socketserver.ThreadingTCPServer.allow_reuse_address = True
    try:
        httpd = socketserver.ThreadingTCPServer((HOST, PORT), Handler)
    except OSError as e:
        print(f"Error: Could not start server on {HOST}:{PORT} - {e}")
        exit(1)

    print(f"Multi-threaded file server is running at http://{HOST}:{PORT}")
    print(f"Serving files from: {os.path.abspath(os.getcwd())}")
    print(f"Total download speed limit: {config.get('throttling', 'total_max_speed', fallback='Unlimited')}")
    print(f"Per-thread speed limit: {config.get('throttling', 'per_thread_max_speed', fallback='Unlimited')}")
    print("Server now supports concurrent multi-threaded downloads (range requests).")

    monitor_thread = Thread(target=monitor_speed, args=(speed_monitor,), daemon=True)
    monitor_thread.start()
    
    control_thread = Thread(target=console_control, daemon=True)
    control_thread.start()

    print("\nPress Ctrl+C to stop the server")

    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down the server...")
        observer.stop()
        httpd.server_close()

    observer.join()