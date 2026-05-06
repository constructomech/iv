use crossbeam_channel::{Receiver, Sender};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

const THUMB_SIZE: u32 = 160;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        LoadBenchCommand::Score(path) => score_file(&path),
        LoadBenchCommand::Directory {
            path,
            runs,
            warmups,
            limit,
        } => run_directory_bench(&path, runs, warmups, limit),
        LoadBenchCommand::File { path, runs } => run_file_bench(&path, runs),
    }
}

enum LoadBenchCommand {
    Score(PathBuf),
    Directory {
        path: PathBuf,
        runs: usize,
        warmups: usize,
        limit: Option<usize>,
    },
    File {
        path: PathBuf,
        runs: usize,
    },
}

fn parse_args(args: &[String]) -> LoadBenchCommand {
    if args.is_empty() || args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_usage_and_exit();
    }

    let mut dir = None;
    let mut runs = 1usize;
    let mut warmups = 1usize;
    let mut limit = None;
    let mut file = None;
    let mut score_path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                if i >= args.len() {
                    print_usage_and_exit();
                }
                dir = Some(PathBuf::from(&args[i]));
            }
            "--runs" => {
                i += 1;
                if i >= args.len() {
                    print_usage_and_exit();
                }
                runs = args[i].parse().unwrap_or_else(|_| {
                    eprintln!("Invalid --runs value: {}", args[i]);
                    std::process::exit(1);
                });
            }
            "--warmups" => {
                i += 1;
                if i >= args.len() {
                    print_usage_and_exit();
                }
                warmups = args[i].parse().unwrap_or_else(|_| {
                    eprintln!("Invalid --warmups value: {}", args[i]);
                    std::process::exit(1);
                });
            }
            "--limit" => {
                i += 1;
                if i >= args.len() {
                    print_usage_and_exit();
                }
                limit = Some(args[i].parse().unwrap_or_else(|_| {
                    eprintln!("Invalid --limit value: {}", args[i]);
                    std::process::exit(1);
                }));
            }
            "--file" => {
                i += 1;
                if i >= args.len() {
                    print_usage_and_exit();
                }
                file = Some(PathBuf::from(&args[i]));
            }
            arg if arg.starts_with('-') => print_usage_and_exit(),
            path => score_path = Some(PathBuf::from(path)),
        }
        i += 1;
    }

    if let Some(path) = dir {
        return LoadBenchCommand::Directory {
            path,
            runs,
            warmups,
            limit,
        };
    }
    if let Some(path) = file {
        return LoadBenchCommand::File { path, runs };
    }
    let Some(path) = score_path else {
        print_usage_and_exit()
    };
    LoadBenchCommand::Score(path)
}

fn score_file(path: &Path) {
    let input = read_input(path);

    let (score, input_kind, tiles, viewport_samples) = match input {
        LoadBenchInput::Fixture(fixture) => {
            if let Err(err) = fixture.validate() {
                eprintln!("Invalid fixture: {err}");
                std::process::exit(1);
            }
            let score = iv::load_bench::score_fixture(&fixture);
            (
                score,
                "fixture",
                Some(fixture.tiles.len()),
                fixture.viewport_timeline.len(),
            )
        }
        LoadBenchInput::ActivityLog(entries) => {
            let viewport_samples = entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.event,
                        iv::load_bench::GridActivityLogEvent::Viewport { .. }
                    )
                })
                .count();
            (
                iv::load_bench::score_activity_log(&entries),
                "activity log",
                None,
                viewport_samples,
            )
        }
    };

    println!("Input: {}", path.display());
    println!("Input kind: {input_kind}");
    if let Some(tiles) = tiles {
        println!("Tiles: {tiles}");
    }
    println!("Viewport samples: {viewport_samples}");
    println!("Weighted blank tile ms: {:.1}", score.weighted_blank_ms);
    println!(
        "First visible texture ms: {}",
        format_optional_ms(score.first_visible_texture_ms)
    );
    println!(
        "Fully nonblank ms: {}",
        format_optional_ms(score.fully_nonblank_ms)
    );
}

enum LoadBenchInput {
    Fixture(iv::load_bench::LoadBenchFixture),
    ActivityLog(Vec<iv::load_bench::GridActivityLogEntry>),
}

fn read_input(path: &Path) -> LoadBenchInput {
    let file = std::fs::File::open(path).unwrap_or_else(|err| {
        eprintln!("Failed to open {}: {err}", path.display());
        std::process::exit(1);
    });
    let value: serde_json::Value = serde_json::from_reader(file).unwrap_or_else(|err| {
        eprintln!("Failed to parse {}: {err}", path.display());
        std::process::exit(1);
    });
    if value.is_array() {
        let entries = serde_json::from_value(value).unwrap_or_else(|err| {
            eprintln!("Failed to parse activity log {}: {err}", path.display());
            std::process::exit(1);
        });
        LoadBenchInput::ActivityLog(entries)
    } else {
        let fixture = serde_json::from_value(value).unwrap_or_else(|err| {
            eprintln!("Failed to parse fixture {}: {err}", path.display());
            std::process::exit(1);
        });
        LoadBenchInput::Fixture(fixture)
    }
}

fn run_directory_bench(path: &Path, runs: usize, warmups: usize, limit: Option<usize>) {
    let files = collect_media_files(path, limit);
    if files.is_empty() {
        eprintln!("No supported media files found in {}", path.display());
        std::process::exit(1);
    }

    println!("Directory: {}", path.display());
    println!("Files: {}", files.len());
    println!("Warmups: {warmups}");
    println!("Runs: {runs}");
    println!("Decode workers: {}", iv::thumbnail_decode_worker_count());
    println!(
        "Media counts: heif={} video={} other={}",
        files
            .iter()
            .filter(|path| iv::is_heif_extension(path))
            .count(),
        files.iter().filter(|path| iv::is_video_file(path)).count(),
        files
            .iter()
            .filter(|path| !iv::is_heif_extension(path) && !iv::is_video_file(path))
            .count()
    );

    for warmup in 1..=warmups {
        let score = run_directory_once(&files);
        println!(
            "warmup={warmup} total_ms={:.1} first_ms={:.1} full_ms={:.1} failures={}",
            score.total_ms, score.first_ms, score.full_ms, score.failures
        );
    }

    let mut scores = Vec::with_capacity(runs);
    for run in 1..=runs {
        let score = run_directory_once(&files);
        if std::env::var("IV_LOAD_BENCH_BY_KIND").is_ok() {
            println!(
                "run={run} total_ms={:.1} first_ms={:.1} full_ms={:.1} failures={} heif_ms={:.1} video_ms={:.1} other_ms={:.1}",
                score.total_ms,
                score.first_ms,
                score.full_ms,
                score.failures,
                score.heif_ms,
                score.video_ms,
                score.other_ms
            );
        } else {
            println!(
                "run={run} total_ms={:.1} first_ms={:.1} full_ms={:.1} failures={}",
                score.total_ms, score.first_ms, score.full_ms, score.failures
            );
        }
        scores.push(score.total_ms);
    }

    scores.sort_by(f64::total_cmp);
    println!("median_ms={:.1}", percentile(&scores, 0.50));
    println!("p95_ms={:.1}", percentile(&scores, 0.95));
    println!("min_ms={:.1}", scores[0]);
    println!("max_ms={:.1}", scores[scores.len() - 1]);
}

fn run_file_bench(path: &Path, runs: usize) {
    for run in 1..=runs {
        let started = Instant::now();
        match decode_thumbnail_like_grid(path) {
            Ok(image) => println!(
                "run={run} ok=true total_ms={:.1} size={}x{}",
                started.elapsed().as_secs_f64() * 1000.0,
                image.width,
                image.height
            ),
            Err(err) => println!(
                "run={run} ok=false total_ms={:.1} error={err}",
                started.elapsed().as_secs_f64() * 1000.0
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct DirectoryRunScore {
    total_ms: f64,
    first_ms: f64,
    full_ms: f64,
    failures: usize,
    heif_ms: f64,
    video_ms: f64,
    other_ms: f64,
}

#[derive(Debug)]
struct BenchResult {
    finished: Instant,
    duration_ms: f64,
    width: u32,
    height: u32,
    kind: BenchKind,
    ok: bool,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
enum BenchKind {
    Heif,
    Video,
    Other,
}

fn run_directory_once(files: &[PathBuf]) -> DirectoryRunScore {
    let worker_count = iv::thumbnail_decode_worker_count();
    let (work_tx, work_rx) = crossbeam_channel::unbounded::<PathBuf>();
    let (result_tx, result_rx) = crossbeam_channel::unbounded::<BenchResult>();
    let start = Instant::now();

    let workers = spawn_decode_workers(worker_count, work_rx, result_tx);
    for path in files {
        work_tx.send(path.clone()).unwrap();
    }
    drop(work_tx);

    let mut first_ms = None;
    let mut full_ms: f64 = 0.0;
    let mut failures = 0usize;
    let mut failed_paths = Vec::new();
    let mut by_ext = BTreeMap::<String, (usize, f64)>::new();
    let mut slowest = Vec::<BenchResult>::new();
    let mut heif_ms = 0.0;
    let mut video_ms = 0.0;
    let mut other_ms = 0.0;
    for _ in 0..files.len() {
        let result = result_rx.recv().unwrap();
        if !result.ok {
            failures += 1;
            failed_paths.push(result.path.clone());
        }
        let elapsed = result.finished.duration_since(start).as_secs_f64() * 1000.0;
        first_ms.get_or_insert(elapsed);
        full_ms = full_ms.max(elapsed);
        match result.kind {
            BenchKind::Heif => heif_ms += result.duration_ms,
            BenchKind::Video => video_ms += result.duration_ms,
            BenchKind::Other => other_ms += result.duration_ms,
        }
        if std::env::var("IV_LOAD_BENCH_BY_EXT").is_ok() {
            let ext = result
                .path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .unwrap_or_else(|| "<none>".into());
            let entry = by_ext.entry(ext).or_default();
            entry.0 += 1;
            entry.1 += result.duration_ms;
            slowest.push(result);
        }
    }
    for worker in workers {
        worker.join().unwrap();
    }

    if std::env::var("IV_LOAD_BENCH_PRINT_FAILURES").is_ok() {
        for path in &failed_paths {
            eprintln!("failed: {}", path.display());
        }
    }

    if std::env::var("IV_LOAD_BENCH_BY_EXT").is_ok() {
        println!("by_ext:");
        for (ext, (count, ms)) in by_ext {
            println!(
                "  {ext} count={count} total_ms={ms:.1} avg_ms={:.1}",
                ms / count as f64
            );
        }
        slowest.sort_by(|a, b| b.duration_ms.total_cmp(&a.duration_ms));
        println!("slowest:");
        for result in slowest.iter().take(20) {
            println!(
                "  {:.1}ms {}x{} {}",
                result.duration_ms,
                result.width,
                result.height,
                result.path.display()
            );
        }
    }

    DirectoryRunScore {
        total_ms: start.elapsed().as_secs_f64() * 1000.0,
        first_ms: first_ms.unwrap_or(0.0),
        full_ms,
        failures,
        heif_ms,
        video_ms,
        other_ms,
    }
}

fn spawn_decode_workers(
    count: usize,
    work_rx: Receiver<PathBuf>,
    result_tx: Sender<BenchResult>,
) -> Vec<thread::JoinHandle<()>> {
    (0..count)
        .map(|_| {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            thread::spawn(move || {
                while let Ok(path) = work_rx.recv() {
                    let kind = bench_kind(&path);
                    let started = Instant::now();
                    let result = decode_thumbnail_like_grid(&path);
                    let finished = Instant::now();
                    let (ok, width, height) = match result {
                        Ok(image) => (true, image.width, image.height),
                        Err(_) => (false, 0, 0),
                    };
                    let _ = result_tx.send(BenchResult {
                        finished,
                        duration_ms: finished.duration_since(started).as_secs_f64() * 1000.0,
                        width,
                        height,
                        kind,
                        ok,
                        path,
                    });
                }
            })
        })
        .collect()
}

fn bench_kind(path: &Path) -> BenchKind {
    if iv::is_video_file(path) {
        BenchKind::Video
    } else if iv::is_heif_extension(path) {
        BenchKind::Heif
    } else {
        BenchKind::Other
    }
}

fn decode_thumbnail_like_grid(path: &Path) -> Result<iv::DecodedImage, String> {
    if iv::is_video_file(path) {
        iv::decode_video_thumbnail(path, THUMB_SIZE)
    } else if iv::is_heif_extension(path) {
        if let Some(image) = iv::try_heif_thumbnail(path) {
            Ok(image)
        } else {
            iv::decode_thumbnail(path, THUMB_SIZE)
        }
    } else {
        match iv::decode_thumbnail_progressive(path, THUMB_SIZE) {
            Ok((image, _, _)) => Ok(image),
            Err(err) => Err(err),
        }
    }
}

fn collect_media_files(path: &Path, limit: Option<usize>) -> Vec<PathBuf> {
    let mut files = std::fs::read_dir(path)
        .unwrap_or_else(|err| {
            eprintln!("Failed to read {}: {err}", path.display());
            std::process::exit(1);
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_file()))
        .map(|entry| entry.path())
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
        })
        .filter(|path| iv::is_media_file(path))
        .collect::<Vec<_>>();
    files.sort_by(|a, b| {
        let a = a
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let b = b
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        a.cmp(&b)
    });
    if let Some(limit) = limit {
        files.truncate(limit);
    }
    files
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn format_optional_ms(value: Option<f64>) -> String {
    value
        .map(|ms| format!("{ms:.1}"))
        .unwrap_or_else(|| "n/a".into())
}

fn print_usage_and_exit() -> ! {
    eprintln!("Usage: iv-load-bench <fixture-or-activity-log.json>");
    eprintln!("       iv-load-bench --dir <folder> [--runs N] [--warmups N] [--limit N]");
    eprintln!("       iv-load-bench --file <media-file> [--runs N]");
    std::process::exit(1);
}
