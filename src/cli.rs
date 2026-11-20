//! Command-line interface. Edits the same config.json the GUI does, so the
//! helper applies changes instantly and an open Vision window stays in sync.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use vision::color::Field;
use vision::{Adjust, Config, Preset, State, Status};

const USAGE: &str = "\
vision: display color control

USAGE
    vision                         open the window
    vision <command> [options]

COMMANDS
    status                         show what is applied where
    on | off | toggle              turn all adjustments on or off
    set <field> <value>            set a value       (vision set saturation 140%)
    adjust <field> <delta>         nudge a value     (vision adjust temperature -500)
    get [field]                    print current values
    reset                          back to stock colors
    preset [list]                  list presets
    preset <name>                  apply a preset    (vision preset vivid)
    preset save <name>             save current settings as a preset
    preset delete <name>           delete one of your presets
    outputs                        list connected displays
    setup                          one-time permission grant (asks for sudo)
    completions fish               print fish completions

OPTIONS
    -o, --output <name>            only this display (repeatable); default: all
    -h, --help                     this help

FIELDS
    saturation  hue  temperature  brightness  contrast  gamma  red  green  blue
    Values use the same units as the window: 140%, -20, 5000K, 1.2.";

struct Args {
    positional: Vec<String>,
    outputs: Vec<String>,
    names: bool,
    user_only: bool,
}

fn parse_args(args: Vec<String>) -> Result<Args, String> {
    let mut a = Args { positional: vec![], outputs: vec![], names: false, user_only: false };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-o" | "--output" => a.outputs.push(it.next().ok_or("--output needs a display name")?),
            "-a" | "--all" => a.outputs.clear(),
            "--names" => a.names = true,
            "--user" => a.user_only = true,
            "-h" | "--help" => a.positional.insert(0, "help".into()),
            s if s.starts_with("--output=") => a.outputs.push(s["--output=".len()..].to_string()),
            // Negative numbers ("-20") are values, not options.
            s if s.starts_with('-') && s.len() > 1 && !s[1..].starts_with(|c: char| c.is_ascii_digit() || c == '.') => {
                return Err(format!("unknown option {s} (see vision --help)"));
            }
            _ => a.positional.push(arg),
        }
    }
    Ok(a)
}

pub fn run(args: Vec<String>) -> i32 {
    match parse_args(args).and_then(dispatch) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("vision: {e}");
            1
        }
    }
}

fn dispatch(a: Args) -> Result<(), String> {
    let cmd = a.positional.first().map(String::as_str).unwrap_or("help");
    let rest = &a.positional[1.min(a.positional.len())..];
    match cmd {
        "help" => {
            println!("{USAGE}");
            Ok(())
        }
        "status" => status(),
        "on" | "off" | "toggle" => power(cmd),
        "set" | "adjust" => set(&a, cmd == "adjust", rest),
        "get" => get(&a, rest),
        "reset" => {
            let mut cfg = load()?;
            for t in targets(&a, &cfg)? {
                cfg.outputs.remove(&t);
                println!("{t}: stock colors");
            }
            save(&cfg)
        }
        "preset" | "presets" => preset(&a, rest),
        "outputs" => {
            for (name, model) in known_outputs(&load()?) {
                if a.names {
                    println!("{name}");
                } else {
                    println!("{name}\t{}", model.unwrap_or_default());
                }
            }
            Ok(())
        }
        "setup" => setup(),
        "completions" => match rest.first().map(String::as_str) {
            Some("fish") | None => {
                print!("{}", fish_completions());
                Ok(())
            }
            Some(other) => Err(format!("no completions for {other} (only fish)")),
        },
        other => Err(format!("unknown command {other:?} (see vision --help)")),
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn load() -> Result<Config, String> {
    Config::load().map_err(|e| format!("can't read {}: {e}", vision::config_path().display()))
}

/// Save, make sure the helper is running, and point out anything that
/// would stop the change from showing up.
fn save(cfg: &Config) -> Result<(), String> {
    cfg.save().map_err(|e| format!("can't save settings: {e}"))?;
    ensure_daemon();
    if !cfg.enabled {
        eprintln!("note: Vision is off; run `vision on` to see changes");
    }
    if Status::load().is_some_and(|s| s.state == State::NeedsPrivilege) {
        eprintln!("note: the helper needs a one-time permission grant; run `vision setup`");
    }
    Ok(())
}

fn ensure_daemon() {
    if !vision::daemon_running() {
        crate::spawn_daemon();
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Connected displays as reported by the helper, falling back to the ones
/// already in the config.
fn known_outputs(cfg: &Config) -> Vec<(String, Option<String>)> {
    let live: Vec<(String, Option<String>)> = Status::load()
        .filter(|_| vision::daemon_running())
        .map(|s| s.outputs.into_iter().map(|o| (o.name, o.model)).collect())
        .unwrap_or_default();
    if live.is_empty() {
        cfg.outputs.keys().map(|k| (k.clone(), None)).collect()
    } else {
        live
    }
}

fn targets(a: &Args, cfg: &Config) -> Result<Vec<String>, String> {
    if !a.outputs.is_empty() {
        return Ok(a.outputs.clone());
    }
    ensure_daemon();
    let all: Vec<String> = known_outputs(cfg).into_iter().map(|o| o.0).collect();
    if all.is_empty() {
        Err("no displays known yet; check `vision status`, or name one with -o eDP-1".into())
    } else {
        Ok(all)
    }
}

fn field_arg(s: Option<&String>) -> Result<Field, String> {
    let s = s.ok_or("which field? (saturation, hue, temperature, brightness, contrast, gamma, red, green, blue)")?;
    Field::from_name(s).ok_or_else(|| format!("unknown field {s:?}"))
}

fn store(cfg: &mut Config, output: &str, adj: Adjust) {
    if adj.is_identity() {
        cfg.outputs.remove(output);
    } else {
        cfg.outputs.insert(output.to_string(), adj);
    }
}

fn summary(adj: &Adjust) -> String {
    let parts: Vec<String> = Field::ALL
        .into_iter()
        .filter(|f| (f.get(adj) - f.default_value()).abs() > 1e-6)
        .map(|f| format!("{} {}", f.name(), f.format(f.get(adj))))
        .collect();
    if parts.is_empty() { "stock".into() } else { parts.join(", ") }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn status() -> Result<(), String> {
    let cfg = load()?;
    let running = vision::daemon_running();
    let st = Status::load().filter(|_| running);
    let mut line = format!("Vision is {}", if cfg.enabled { "on" } else { "off" });
    match &st {
        None => line.push_str(" · helper not running (run `vision` or any command to start it)"),
        Some(s) => {
            let _ = match s.state {
                State::Ok => write!(line, " · applied in display hardware"),
                State::NeedsPrivilege => write!(line, " · helper needs one-time setup: run `vision setup`"),
                _ => write!(line, " · {}", s.message),
            };
            if !s.compositor.is_empty() {
                let _ = write!(line, " ({})", s.compositor);
            }
        }
    }
    println!("{line}");
    if let Some(w) = st.as_ref().and_then(|s| s.warning.as_ref()) {
        println!("warning: {w}");
    }
    let outs = known_outputs(&cfg);
    for (name, model) in &outs {
        let adj = cfg.outputs.get(name).copied().unwrap_or_default();
        let driver = st
            .as_ref()
            .and_then(|s| s.outputs.iter().find(|o| &o.name == name))
            .map(|o| format!(" [{}]", o.driver))
            .unwrap_or_default();
        let model = model.as_ref().map(|m| format!(" ({m})")).unwrap_or_default();
        println!("  {name}{model}{driver}: {}", summary(&adj));
    }
    if outs.is_empty() {
        println!("  no displays known yet");
    }
    Ok(())
}

fn power(cmd: &str) -> Result<(), String> {
    let mut cfg = load()?;
    cfg.enabled = match cmd {
        "on" => true,
        "off" => false,
        _ => !cfg.enabled,
    };
    println!("Vision {}", if cfg.enabled { "on" } else { "off" });
    save(&cfg)
}

fn set(a: &Args, relative: bool, rest: &[String]) -> Result<(), String> {
    let field = field_arg(rest.first())?;
    let text = rest.get(1).ok_or_else(|| {
        format!("{} what? e.g. vision {} {} {}", if relative { "adjust by" } else { "set to" },
            if relative { "adjust" } else { "set" }, field.name(), if relative { "+10" } else { "140%" })
    })?;
    let mut cfg = load()?;
    for t in targets(a, &cfg)? {
        let mut adj = cfg.outputs.get(&t).copied().unwrap_or_default();
        let v = if relative {
            let d = field.parse_raw(text).ok_or_else(|| format!("bad amount {text:?}"))?;
            field.snap(field.get(&adj) + d)
        } else {
            field.parse(text).ok_or_else(|| format!("bad value {text:?}"))?
        };
        field.set(&mut adj, v);
        println!("{t}: {} {}", field.name(), field.format(field.get(&adj)));
        store(&mut cfg, &t, adj);
    }
    save(&cfg)
}

fn get(a: &Args, rest: &[String]) -> Result<(), String> {
    let cfg = load()?;
    let field = rest.first().map(|s| field_arg(Some(s))).transpose()?;
    let ts = targets(a, &cfg)?;
    for t in &ts {
        let adj = cfg.outputs.get(t).copied().unwrap_or_default();
        match field {
            // A single value for a single display prints bare, for scripts.
            Some(f) if ts.len() == 1 => println!("{}", f.format(f.get(&adj))),
            Some(f) => println!("{t}: {}", f.format(f.get(&adj))),
            None => {
                println!("{t}:");
                for f in Field::ALL {
                    println!("  {:<12}{}", f.name(), f.format(f.get(&adj)));
                }
            }
        }
    }
    Ok(())
}

fn preset(a: &Args, rest: &[String]) -> Result<(), String> {
    let mut cfg = load()?;
    match rest.first().map(String::as_str) {
        None | Some("list") => {
            if !a.user_only {
                for (name, adj) in vision::builtin_presets() {
                    if a.names { println!("{}", name.to_lowercase()) } else { println!("  {name:<14}{}", summary(&adj)) }
                }
            }
            for p in &cfg.presets {
                if a.names { println!("{}", p.name) } else { println!("  {:<14}{}  (yours)", p.name, summary(&p.adjust)) }
            }
            Ok(())
        }
        Some("save") => {
            let name = rest[1..].join(" ");
            if name.trim().is_empty() {
                return Err("preset save needs a name".into());
            }
            let t = targets(a, &cfg)?;
            let adj = cfg.outputs.get(&t[0]).copied().unwrap_or_default();
            match cfg.presets.iter_mut().find(|p| p.name == name) {
                Some(p) => p.adjust = adj,
                None => cfg.presets.push(Preset { name: name.clone(), adjust: adj }),
            }
            println!("saved \"{name}\" from {}: {}", t[0], summary(&adj));
            save(&cfg)
        }
        Some("delete") | Some("rm") => {
            let name = rest[1..].join(" ");
            let before = cfg.presets.len();
            cfg.presets.retain(|p| !p.name.eq_ignore_ascii_case(&name));
            if cfg.presets.len() == before {
                return Err(format!("no preset of yours called {name:?}"));
            }
            println!("deleted \"{name}\"");
            save(&cfg)
        }
        Some(_) => {
            let name = rest.join(" ");
            let adj = cfg
                .presets
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(&name))
                .map(|p| p.adjust)
                .or_else(|| vision::builtin_presets().into_iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)).map(|p| p.1))
                .ok_or_else(|| format!("no preset called {name:?} (see vision preset list)"))?;
            for t in targets(a, &cfg)? {
                store(&mut cfg, &t, adj);
                println!("{t}: {name} ({})", summary(&adj));
            }
            save(&cfg)
        }
    }
}

pub(crate) fn setcap_path() -> Option<PathBuf> {
    ["/usr/bin/setcap", "/usr/sbin/setcap", "/sbin/setcap"].iter().map(PathBuf::from).find(|p| p.exists())
}

/// Interactive version of the GUI's one-time setup.
fn setup() -> Result<(), String> {
    let daemon = crate::visiond_path();
    if !daemon.is_absolute() {
        return Err("can't find visiond next to the vision binary".into());
    }
    let setcap = setcap_path().ok_or("setcap not found (install libcap)")?;
    println!("Granting visiond permission to program the display hardware (sudo):");
    let ok = Command::new("sudo")
        .arg(&setcap)
        .arg("cap_sys_ptrace=ep")
        .arg(&daemon)
        .status()
        .map_err(|e| format!("couldn't run sudo: {e}"))?
        .success();
    if !ok {
        return Err("sudo setcap failed".into());
    }
    if let Some(s) = Status::load().filter(|_| vision::daemon_running()) {
        unsafe { libc::kill(s.pid as i32, libc::SIGTERM) };
        std::thread::sleep(Duration::from_millis(200));
    }
    crate::spawn_daemon();
    std::thread::sleep(Duration::from_millis(500));
    println!("Done.");
    status()
}

// ---------------------------------------------------------------------------
// fish completions
// ---------------------------------------------------------------------------

fn fish_completions() -> String {
    let cmds: &[(&str, &str)] = &[
        ("status", "Show what is applied where"),
        ("on", "Turn adjustments on"),
        ("off", "Turn adjustments off (stock colors)"),
        ("toggle", "Toggle adjustments on/off"),
        ("set", "Set a value"),
        ("adjust", "Nudge a value (+10, -500)"),
        ("get", "Print current values"),
        ("reset", "Back to stock colors"),
        ("preset", "List, apply, save or delete presets"),
        ("outputs", "List connected displays"),
        ("setup", "One-time permission grant (sudo)"),
        ("completions", "Print shell completions"),
        ("help", "Show help"),
    ];
    let names: Vec<&str> = cmds.iter().map(|c| c.0).collect();
    let fields: Vec<&str> = Field::ALL.iter().map(|f| f.name()).collect();
    let mut s = String::new();
    let _ = writeln!(s, "# fish completions for vision (written automatically by Vision)");
    let _ = writeln!(s, "set -l cmds {}", names.join(" "));
    let _ = writeln!(s, "set -l fields {}", fields.join(" "));
    let _ = writeln!(s, "complete -c vision -f");
    for (c, d) in cmds {
        let _ = writeln!(s, "complete -c vision -n \"not __fish_seen_subcommand_from $cmds\" -a {c} -d '{d}'");
    }
    // Fields after set / adjust / get.
    for f in Field::ALL {
        let _ = writeln!(
            s,
            "complete -c vision -n \"__fish_seen_subcommand_from set adjust get; and not __fish_seen_subcommand_from $fields\" -a {} -d '{}'",
            f.name(),
            f.describe()
        );
    }
    // Example values after `set <field>`.
    let examples: &[(Field, &[(&str, &str)])] = &[
        (Field::Saturation, &[("100%", "stock"), ("120%", "a bit richer"), ("140%", "vivid"), ("170%", "punchy"), ("0%", "grayscale")]),
        (Field::Hue, &[("0", "stock")]),
        (Field::Temperature, &[("6500", "stock"), ("5500", "warm"), ("4500", "warmer"), ("3400", "night"), ("8000", "cool")]),
        (Field::Brightness, &[("0", "stock")]),
        (Field::Contrast, &[("100%", "stock"), ("110%", "more contrast")]),
        (Field::Gamma, &[("1.00", "stock")]),
        (Field::Red, &[("100%", "stock")]),
        (Field::Green, &[("100%", "stock")]),
        (Field::Blue, &[("100%", "stock")]),
    ];
    for (f, vals) in examples {
        for (v, d) in *vals {
            let _ = writeln!(
                s,
                "complete -c vision -n \"__fish_seen_subcommand_from set; and __fish_seen_subcommand_from {}\" -a '{v}' -d '{d}'",
                f.name()
            );
        }
    }
    let _ = writeln!(
        s,
        "complete -c vision -n \"__fish_seen_subcommand_from adjust; and __fish_seen_subcommand_from $fields\" -a '+10 -10 +500 -500' -d 'amount'"
    );
    // Presets.
    let _ = writeln!(
        s,
        "complete -c vision -n \"__fish_seen_subcommand_from preset presets; and not __fish_seen_subcommand_from list save delete rm\" -a '(vision preset list --names 2>/dev/null)' -d preset"
    );
    let _ = writeln!(
        s,
        "complete -c vision -n \"__fish_seen_subcommand_from preset presets; and not __fish_seen_subcommand_from list save delete rm\" -a 'list save delete' -d action"
    );
    let _ = writeln!(
        s,
        "complete -c vision -n \"__fish_seen_subcommand_from preset; and __fish_seen_subcommand_from delete rm\" -a '(vision preset list --names --user 2>/dev/null)' -d 'your preset'"
    );
    let _ = writeln!(s, "complete -c vision -n \"__fish_seen_subcommand_from completions\" -a fish");
    // Options.
    let _ = writeln!(
        s,
        "complete -c vision -s o -l output -x -a '(vision outputs --names 2>/dev/null)' -d 'Only this display'"
    );
    let _ = writeln!(s, "complete -c vision -s h -l help -d 'Show help'");
    s
}

/// Keep fish completions current wherever fish will find them, without
/// touching the user's own ~/.config/fish.
pub fn install_fish_completions() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    let fish_present = home.join(".config/fish").exists() || data.join("fish").exists();
    if !fish_present {
        return;
    }
    let path = data.join("fish/vendor_completions.d/vision.fish");
    let script = fish_completions();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(script.as_str()) {
        let _ = vision::write_atomic(&path, script.as_bytes());
    }
}
