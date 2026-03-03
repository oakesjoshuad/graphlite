use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{unbounded, Sender};

use crate::{config, viz, watcher_engine};

const SSE_SCRIPT: &str = r#"<script>
(function(){
  var es = new EventSource('/events');
  es.onmessage = function(){ location.reload(); };
})();
</script>"#;

pub fn run(root: &str, port: u16, lsp: bool) -> Result<()> {
    let root = config::find_root(root).unwrap_or_else(|_| root.to_string());

    // Change into the root so viz::generate_html() can find .graphlite/
    std::env::set_current_dir(&root)?;

    // Generate initial HTML.
    let initial_html = inject_sse(viz::generate_html()?);
    let html_state: Arc<RwLock<String>> = Arc::new(RwLock::new(initial_html));

    // SSE client senders: each connected browser gets its own channel.
    let sse_clients: Arc<Mutex<Vec<Sender<()>>>> = Arc::new(Mutex::new(Vec::new()));

    // Determine LSP languages from config (only if --lsp flag passed).
    let lsp_langs: Vec<String> = if lsp {
        config::load(".").lsp.clone()
    } else {
        Vec::new()
    };

    // Start the watcher engine.
    let engine = watcher_engine::start(root.clone(), lsp_langs)?;

    // Thread: receive engine change signals, regenerate HTML, broadcast SSE.
    {
        let html_state = html_state.clone();
        let sse_clients = sse_clients.clone();
        let changed_rx = engine.changed_rx;
        thread::spawn(move || {
            while changed_rx.recv().is_ok() {
                match viz::generate_html() {
                    Ok(new_html) => {
                        let injected = inject_sse(new_html);
                        if let Ok(mut guard) = html_state.write() {
                            *guard = injected;
                        }
                    }
                    Err(e) => eprintln!("[serve] viz regeneration error: {}", e),
                }
                // Broadcast reload to all connected SSE clients.
                if let Ok(mut clients) = sse_clients.lock() {
                    clients.retain(|tx| tx.send(()).is_ok());
                }
            }
        });
    }

    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr)?;
    eprintln!("[serve] http://{}  (Ctrl-C to stop)", addr);
    eprintln!("[serve] lsp enrichment: {}", lsp);

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let html_state = html_state.clone();
                let sse_clients = sse_clients.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_connection(s, html_state, sse_clients) {
                        eprintln!("[serve] connection error: {}", e);
                    }
                });
            }
            Err(e) => eprintln!("[serve] accept error: {}", e),
        }
    }
    Ok(())
}

fn inject_sse(html: String) -> String {
    html.replace("</body>", &format!("{}</body>", SSE_SCRIPT))
}

fn handle_connection(
    stream: TcpStream,
    html_state: Arc<RwLock<String>>,
    sse_clients: Arc<Mutex<Vec<Sender<()>>>>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Drain remaining headers (we only need the request line).
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header == "\r\n" || header.is_empty() {
            break;
        }
    }

    let method_path: Vec<&str> = request_line.splitn(3, ' ').collect();
    if method_path.len() < 2 {
        return Ok(());
    }
    let path = method_path[1];

    match path {
        "/" | "/index.html" => serve_html(stream, html_state),
        "/events" => serve_sse(stream, sse_clients),
        _ => serve_404(stream),
    }
}

fn serve_html(mut stream: TcpStream, html_state: Arc<RwLock<String>>) -> Result<()> {
    let body = html_state.read().unwrap().clone();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn serve_sse(mut stream: TcpStream, sse_clients: Arc<Mutex<Vec<Sender<()>>>>) -> Result<()> {
    // SSE connections are long-lived; disable the read timeout.
    stream.set_read_timeout(None)?;

    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n";
    stream.write_all(headers.as_bytes())?;

    let (tx, rx) = unbounded::<()>();
    if let Ok(mut clients) = sse_clients.lock() {
        clients.push(tx);
    }

    let heartbeat = Duration::from_secs(15);
    loop {
        match rx.recv_timeout(heartbeat) {
            Ok(()) => {
                if stream.write_all(b"data: reload\n\n").is_err() {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if stream.write_all(b": ping\n\n").is_err() {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn serve_404(mut stream: TcpStream) -> Result<()> {
    stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    Ok(())
}
