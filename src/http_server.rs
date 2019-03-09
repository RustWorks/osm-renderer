use crate::errors::*;

use crate::draw::drawer::Drawer;
use crate::geodata::reader::GeodataReader;
use crate::mapcss::parser::parse_file;
use crate::mapcss::styler::{StyleType, Styler};
use crate::perf_stats::PerfStats;
use crate::tile::{Tile, MAX_ZOOM};
use error_chain::bail;
use num_cpus;
use std::collections::HashSet;
use std::io::prelude::*;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;

#[cfg_attr(feature = "cargo-clippy", allow(clippy::implicit_hasher))]
pub fn run_server(
    address: &str,
    geodata_file: &str,
    stylesheet_file: &str,
    stylesheet_type: &StyleType,
    font_size_multiplier: Option<f64>,
    osm_ids: Option<HashSet<u64>>,
) -> Result<()> {
    let (base_path, file_name) = split_stylesheet_path(stylesheet_file)?;
    let rules = parse_file(&base_path, &file_name).chain_err(|| "Failed to parse the stylesheet file")?;

    let server = Arc::new(HttpServer {
        styler: Styler::new(rules, stylesheet_type, font_size_multiplier),
        reader: GeodataReader::load(geodata_file).chain_err(|| "Failed to load the geodata file")?,
        drawer: Drawer::new(&base_path),
        osm_ids,
        perf_stats: Mutex::new(PerfStats::default()),
    });

    let thread_count = num_cpus::get();

    let mut senders: Vec<Sender<TcpStream>> = Vec::new();
    let mut receivers: Vec<Receiver<TcpStream>> = Vec::new();

    for _ in 0..thread_count {
        let (tx, rx) = mpsc::channel();
        senders.push(tx);
        receivers.push(rx);
    }

    let mut handlers = Vec::new();

    for receiver in receivers {
        let server_ref = Arc::clone(&server);
        handlers.push(thread::spawn(move || {
            while let Ok(stream) = receiver.recv() {
                server_ref.handle_connection(stream);
            }
        }));
    }

    let tcp_listener = TcpListener::bind(address).chain_err(|| format!("Failed to bind to {}", address))?;
    let mut thread_id = 0;

    for tcp_stream in tcp_listener.incoming() {
        if let Ok(stream) = tcp_stream {
            senders[thread_id].send(stream).unwrap();
            thread_id = (thread_id + 1) % senders.len();
        }
    }

    for h in handlers {
        h.join().unwrap();
    }

    Ok(())
}

struct HttpServer<'a> {
    styler: Styler,
    reader: GeodataReader<'a>,
    drawer: Drawer,
    osm_ids: Option<HashSet<u64>>,
    perf_stats: Mutex<PerfStats>,
}

impl<'a> HttpServer<'a> {
    fn handle_connection(&self, stream: TcpStream) {
        let peer_addr = stream.peer_addr();
        match self.try_handle_connection(stream) {
            Ok(_) => {}
            Err(e) => {
                let peer_addr_str = match peer_addr {
                    Ok(addr) => format!(" from {}", addr),
                    _ => String::new(),
                };
                eprintln!("Error processing request{}: {}", peer_addr_str, e)
            }
        }
    }

    fn try_handle_connection(&self, stream: TcpStream) -> Result<()> {
        let mut rdr = BufReader::new(stream);

        let first_line = match rdr.by_ref().lines().next() {
            Some(Ok(line)) => line,
            _ => bail!("Failed to read the first line from the TCP stream"),
        };

        let path = extract_path_from_request(&first_line)?;

        if cfg!(feature = "perf-stats") && path == "/perf_stats" {
            let perf_stats_html = self.perf_stats.lock().unwrap().to_html();
            serve_data(&mut rdr.into_inner(), perf_stats_html.as_bytes(), "text/html");
            return Ok(());
        }

        let tile = match extract_tile_from_path(&path) {
            Some(tile) => tile,
            _ => bail!("<{}> doesn't look like a valid tile ID", path),
        };

        if cfg!(feature = "perf-stats") {
            crate::perf_stats::start_tile(tile.zoom);
        }

        let entities = {
            let _m = crate::perf_stats::measure("Get tile entities");
            self.reader.get_entities_in_tile_with_neighbors(&tile, &self.osm_ids)
        };
        let tile_png_bytes = self.drawer.draw_tile(&entities, &tile, &self.styler).unwrap();

        if cfg!(feature = "perf-stats") {
            crate::perf_stats::finish_tile(&mut self.perf_stats.lock().unwrap());
        }

        serve_data(&mut rdr.into_inner(), &tile_png_bytes, "image/png");

        Ok(())
    }
}

fn serve_data(stream: &mut TcpStream, data: &[u8], content_type: &str) {
    let header = [
        "HTTP/1.1 200 OK",
        &format!("Content-Type: {}", content_type),
        &format!("Content-Length: {}", data.len()),
        "Connection: close",
        "",
        "",
    ]
    .join("\r\n");

    // Errors at this stage usually happen when the outstanding requests get terminated for some
    // reason (e.g. the user scrolls the map). We're not interested in reporting these errors,
    // but there's no point in continuing after a write fails either.
    if stream.write_all(header.as_bytes()).is_ok() {
        let _ = stream.write_all(&data);
    }
}

fn extract_path_from_request(first_line: &str) -> Result<String> {
    let tokens: Vec<_> = first_line.split(' ').collect();
    if tokens.len() != 3 {
        bail!("<{}> doesn't look like a valid HTTP request", first_line);
    }
    let method = tokens[0];
    if method != "GET" {
        bail!("Invalid HTTP method: {}", method);
    }
    let http_version = tokens[2];
    if http_version != "HTTP/1.1" && http_version != "HTTP/1.0" {
        bail!("Invalid HTTP version: {}", http_version);
    }
    Ok(tokens[1].to_string())
}

fn extract_tile_from_path(path: &str) -> Option<Tile> {
    let expected_token_count = 3;

    let real_path = match path.rfind('?') {
        Some(pos) => &path[..pos],
        None => path,
    };

    let mut tokens = real_path
        .trim_end_matches(".png")
        .rsplit('/')
        .take(expected_token_count)
        .collect::<Vec<_>>();

    if tokens.len() != expected_token_count {
        return None;
    }

    tokens.reverse();
    let (z_str, x_str, y_str) = (tokens[0], tokens[1], tokens[2]);

    match (z_str.parse(), x_str.parse(), y_str.parse()) {
        (Ok(z), Ok(x), Ok(y)) if z <= MAX_ZOOM => Some(Tile { zoom: z, x, y }),
        _ => None,
    }
}

fn split_stylesheet_path(file_path: &str) -> Result<(PathBuf, String)> {
    let mut result = PathBuf::from(file_path);
    let file_name = result
        .file_name()
        .and_then(|x| x.to_str().map(|y| y.to_string()))
        .ok_or_else(|| ErrorKind::Msg(format!("Failed to extract the file name for {}", file_path)))?;
    result.pop();
    Ok((result, file_name))
}
