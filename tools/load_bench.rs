use std::path::{Path, PathBuf};

fn main() {
    let fixture_path = parse_args(std::env::args().skip(1).collect());
    let input = read_input(&fixture_path);

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

    println!("Input: {}", fixture_path.display());
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

fn parse_args(args: Vec<String>) -> PathBuf {
    if args.len() != 1 || args[0] == "-h" || args[0] == "--help" {
        print_usage_and_exit();
    }
    PathBuf::from(&args[0])
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

fn format_optional_ms(value: Option<f64>) -> String {
    value
        .map(|ms| format!("{ms:.1}"))
        .unwrap_or_else(|| "n/a".into())
}

fn print_usage_and_exit() -> ! {
    eprintln!("Usage: iv-load-bench <fixture-or-activity-log.json>");
    std::process::exit(1);
}
