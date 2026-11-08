//! `lz4r` — the command line tool, ported from the reference `lz4` (`programs/lz4cli.c`,
//! `programs/lz4io.c`).
//!
//! GPL-2.0-or-later, unlike the library it drives (BSD-2-Clause): see `COPYING`. The two
//! live in separate crates for exactly that reason.
//!
//! What it owns: the argument grammar, the file conventions (`.lz4` suffix, refusing to
//! overwrite, atomic replace via a `.tmp` file, `--rm`), and stdin/stdout. Everything about
//! the format lives in `lz4_in_rust::frame`; everything about threads in
//! `lz4_in_rust::par`; the `-1..-12` scale in `lz4_in_rust::level`.
//!
//! Difference from the reference worth knowing: this reads the whole input into memory
//! (the library's frame layer is whole-buffer so far), so it is not for multi-gigabyte
//! files yet. Files and pipes both work up to the size of the machine's memory.

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lz4_in_rust::frame::{self, BlockMode, Opts as FrameOpts};
use lz4_in_rust::level::Level;
use lz4_in_rust::par::Workers;

const SUFFIX: &str = ".lz4";
const USAGE: &str = "\
lz4r - LZ4 compression for .lz4 files

Usage:
  lz4r [options] [input] [output]

  With one argument, compresses it to <input>.lz4, or decompresses it (with -d) to
  <input> minus the .lz4 suffix. `-` means stdin, and -c means stdout.

Options:
  -1 .. -12      compression level (default 1; 2..12 are high compression)
  -d, --decompress   decompress instead of compress
  -z, --compress     compress (default; cancels -d)
  -c, --stdout       write to stdout, keep the input
  -t, --test         decompress and discard: check integrity only
  -f, --force        overwrite the output, allow console input/output
  -B4 .. -B7         block size: 64 KiB, 256 KiB, 1 MiB, 4 MiB (default 4 MiB)
  -D <file>          dictionary: needed to decode a frame compressed against one
  -BD                blocks depend on the previous one (better ratio, no parallel decode)
  -BX                add a per-block checksum
  --no-frame-crc     no checksum of the whole content (speed, less safety)
  --content-size     write the content size into the frame header
  -T#                worker threads: -T0 = auto, -T1 = one, -T# = that many
  --single-thread    compress with one thread, without splitting into jobs
  --rm               delete the input after a successful write
  -q, --quiet        only errors;  -v, --verbose  more detail
  -h, --help         this message;  -V, --version  version

Exit status: 0 on success, 1 on any failure.
";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let cmd = match parse(&args) {
        Ok(Action::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Action::Version) => {
            println!(
                "lz4r {} (LZ4 port, BSD-2 library + GPL-2 tool)",
                env!("CARGO_PKG_VERSION")
            );
            return ExitCode::SUCCESS;
        }
        Ok(Action::Run(cmd)) => cmd,
        Err(e) => {
            eprintln!("lz4r: {e}");
            eprintln!("try `lz4r -h`");
            return ExitCode::FAILURE;
        }
    };
    match run(&cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lz4r: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------- the command line

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Compress,
    Decompress,
    Test,
}

/// Everything the command line can say. Parsing is separated from doing so the grammar can
/// be tested without touching a file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cmd {
    mode: Mode,
    opts: FrameOpts,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    to_stdout: bool,
    /// `-D`: the dictionary file, needed to decode a frame compressed against one.
    dictionary: Option<PathBuf>,
    force: bool,
    quiet: bool,
    verbose: bool,
    remove_source: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Help,
    Version,
    Run(Cmd),
}

fn default_cmd() -> Cmd {
    Cmd {
        mode: Mode::Compress,
        opts: FrameOpts::default(),
        input: None,
        output: None,
        to_stdout: false,
        dictionary: None,
        force: false,
        quiet: false,
        verbose: false,
        remove_source: false,
    }
}

fn parse(args: &[OsString]) -> Result<Action, String> {
    let mut cmd = default_cmd();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut only_files = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let Some(text) = arg.to_str() else {
            files.push(PathBuf::from(arg));
            continue;
        };

        if only_files || !text.starts_with('-') || text == "-" {
            files.push(PathBuf::from(arg));
            continue;
        }
        if text == "--" {
            only_files = true;
            continue;
        }
        if text == "-D" || text == "--dictionary" {
            let Some(path) = args.get(i) else {
                return Err(format!("{text} needs a file name"));
            };
            cmd.dictionary = Some(PathBuf::from(path));
            i += 1;
            continue;
        }
        if text.starts_with("--") {
            match text {
                "--decompress" => cmd.mode = Mode::Decompress,
                "--compress" => cmd.mode = Mode::Compress,
                "--stdout" | "--to-stdout" => cmd.to_stdout = true,
                "--test" => cmd.mode = Mode::Test,
                "--force" => cmd.force = true,
                "--quiet" => cmd.quiet = true,
                "--verbose" => cmd.verbose = true,
                "--rm" => cmd.remove_source = true,
                "--no-frame-crc" => cmd.opts.content_checksum = false,
                "--content-size" => cmd.opts.content_size = true,
                "--single-thread" => cmd.opts.workers = Workers::Single,
                "--help" => return Ok(Action::Help),
                "--version" => return Ok(Action::Version),
                other => return Err(format!("unknown option {other}")),
            }
            continue;
        }

        // short flags, possibly bundled: -9f, -B4D, -T2
        let body = &text[1..];
        if body.is_empty() {
            files.push(PathBuf::from(arg));
            continue;
        }
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '1'..='9' => {
                    let level = if c == '1' && chars.peek().is_some_and(|n| n.is_ascii_digit()) {
                        // -10..-12
                        let mut digits = String::from(c);
                        while chars.peek().is_some_and(|n| n.is_ascii_digit()) {
                            digits.push(chars.next().unwrap());
                        }
                        digits.parse().map_err(|_| "bad level".to_string())?
                    } else {
                        c as u8 - b'0'
                    };
                    cmd.opts.level = Level::new(level)
                        .ok_or_else(|| format!("level {level} is outside 1..=12"))?;
                }
                'd' => cmd.mode = Mode::Decompress,
                'z' => cmd.mode = Mode::Compress,
                'c' => cmd.to_stdout = true,
                't' => cmd.mode = Mode::Test,
                'f' => cmd.force = true,
                'q' => cmd.quiet = true,
                'v' => cmd.verbose = true,
                'h' => return Ok(Action::Help),
                'V' => return Ok(Action::Version),
                // -B takes a block size id and may carry a modifier: -B4, -B4D, -B4X,
                // or the modifier alone: -BD, -BX
                'B' => {
                    let modifier = |c: char, cmd: &mut Cmd| -> bool {
                        match c {
                            'D' => cmd.opts.block_mode = BlockMode::Linked,
                            'X' => cmd.opts.block_checksum = true,
                            _ => return false,
                        }
                        true
                    };
                    match chars.peek().copied() {
                        Some(id @ '4'..='7') => {
                            chars.next();
                            // 4 -> 64 KiB, 5 -> 256 KiB, 6 -> 1 MiB, 7 -> 4 MiB
                            cmd.opts.block_size = 1usize << (16 + 2 * (id as u8 - b'4') as usize);
                            if let Some(c) = chars.peek().copied() {
                                if modifier(c, &mut cmd) {
                                    chars.next();
                                }
                            }
                        }
                        Some(c) => {
                            if !modifier(c, &mut cmd) {
                                return Err("-B needs a block size (4..7), D or X".to_string());
                            }
                            chars.next();
                        }
                        None => return Err("-B needs a block size (4..7), D or X".to_string()),
                    }
                }
                'T' => {
                    let mut digits = String::new();
                    while chars.peek().is_some_and(|n| n.is_ascii_digit()) {
                        digits.push(chars.next().unwrap());
                    }
                    let n: usize = digits
                        .parse()
                        .map_err(|_| "-T needs a number".to_string())?;
                    cmd.opts.workers = if n == 0 {
                        Workers::Auto
                    } else {
                        Workers::Fixed(n)
                    };
                }
                other => return Err(format!("unknown option -{other}")),
            }
        }
    }

    match files.len() {
        0 => {}
        1 => {
            cmd.input = Some(files.remove(0));
            if !cmd.to_stdout && cmd.mode != Mode::Test {
                cmd.output = Some(default_output(&cmd.input.clone().unwrap(), cmd.mode)?);
            }
        }
        2 => {
            cmd.input = Some(files.remove(0));
            cmd.output = Some(files.remove(0));
        }
        n => return Err(format!("expected at most 2 file names, got {n}")),
    }
    Ok(Action::Run(cmd))
}

/// `foo` -> `foo.lz4`, `foo.lz4` -> `foo`, `foo.txt` (decompressing) -> `foo.txt.out`.
fn default_output(input: &Path, mode: Mode) -> Result<PathBuf, String> {
    match mode {
        Mode::Compress => {
            let mut name = input.as_os_str().to_owned();
            name.push(SUFFIX);
            Ok(PathBuf::from(name))
        }
        Mode::Decompress | Mode::Test => {
            let name = input.as_os_str().to_string_lossy();
            match name.strip_suffix(SUFFIX) {
                Some(stripped) if !stripped.is_empty() => Ok(PathBuf::from(stripped)),
                _ => {
                    let mut name = input.as_os_str().to_owned();
                    name.push(".out");
                    Ok(PathBuf::from(name))
                }
            }
        }
    }
}

// ---------------------------------------------------------------- doing it

/// The two console rules, as a pure function so they can be tested without a terminal:
/// reading a file from a keyboard looks like a hang, and compressed bytes on a console
/// are unreadable garbage (and can wedge some terminals).
fn check_terminals(cmd: &Cmd, stdin_is_tty: bool, stdout_is_tty: bool) -> Result<(), String> {
    if cmd.input.is_none() && stdin_is_tty && !cmd.force {
        return Err("no input given, and stdin is a terminal (see -h)".to_string());
    }
    let writes_stdout =
        cmd.to_stdout || cmd.output.as_deref().map_or(true, |p| p.as_os_str() == "-");
    if writes_stdout && stdout_is_tty && cmd.mode == Mode::Compress && !cmd.force {
        return Err(
            "refusing to write compressed data to a console; use -f or redirect".to_string(),
        );
    }
    if cmd.to_stdout && cmd.output.is_some() && cmd.output.as_deref() != Some(Path::new("-")) {
        eprintln!("lz4r: warning: -c writes to stdout; the output name is ignored");
    }
    Ok(())
}

fn run(cmd: &Cmd) -> Result<(), String> {
    check_terminals(cmd, io::stdin().is_terminal(), io::stdout().is_terminal())?;
    match cmd.mode {
        Mode::Compress => compress_stream(cmd),
        Mode::Decompress => {
            let mut out = open_output(cmd)?;
            let written = decode_into(cmd, &mut out)?;
            out.commit()?;
            if cmd.verbose {
                eprintln!("{}: {written} bytes decompressed", describe_input(cmd));
            }
            remove_source(cmd);
            Ok(())
        }
        Mode::Test => {
            let written = decode_into(cmd, &mut io::sink())?;
            if !cmd.quiet {
                println!("{}: {written} bytes, ok", describe_input(cmd));
            }
            Ok(())
        }
    }
}

/// The input size when it can be known (a regular file), which is what `--content-size`
/// needs and what lets the block size shrink to fit.
fn declared_len(cmd: &Cmd) -> Option<u64> {
    let path = cmd.input.as_ref()?;
    if path.as_os_str() == "-" {
        return None;
    }
    let meta = fs::metadata(path).ok()?;
    meta.is_file().then_some(meta.len())
}

/// Compress the input into a frame, one batch of blocks at a time.
fn compress_stream(cmd: &Cmd) -> Result<(), String> {
    if cmd.opts.block_mode == frame::BlockMode::Linked && !cmd.quiet {
        // docs/DESIGN.md §11: the extDict encoder that would let blocks reference each other
        // is not ported yet, so the frame says "linked" and its blocks stay independent
        eprintln!(
            "lz4r: note: -BD marks the frame as linked, but blocks do not reference each other yet"
        );
    }
    if cmd.dictionary.is_some() {
        return Err(
            "compressing with a dictionary (-D) is not ported yet; it decodes fine".to_string(),
        );
    }
    let len = declared_len(cmd);
    // The reference tool shrinks the block size to the smallest one that holds the whole
    // input (LZ4F_optimalBSID): a 15 KiB file gets 64 KiB blocks even with -B7. Same ratio,
    // less memory, and the frames stay comparable.
    let block_size = match len {
        // a >4 GiB file on a 32-bit host must not wrap into a tiny value
        Some(len) => optimal_block_size(
            cmd.opts.block_size,
            usize::try_from(len).unwrap_or(usize::MAX),
        ),
        None => cmd.opts.block_size,
    };
    let opts = frame::Opts {
        block_size,
        ..cmd.opts
    };

    let mut input = open_input(cmd)?;
    let mut out = open_output(cmd)?;
    let mut enc =
        frame::Encoder::new(io::BufWriter::new(&mut out), &opts, len).map_err(|e| e.to_string())?;
    io::copy(&mut input, &mut enc).map_err(|e| format!("{}: {e}", describe_input(cmd)))?;
    // a Writer has no end-of-stream call: the end mark is explicit
    let mut buffered = enc.finish().map_err(|e| e.to_string())?;
    buffered.flush().map_err(|e| e.to_string())?;
    drop(buffered);
    out.commit()?;
    remove_source(cmd);
    Ok(())
}

/// Decode every frame in the input into `out`, returning the bytes written.
fn decode_into(cmd: &Cmd, out: &mut dyn Write) -> Result<u64, String> {
    let mut input = open_input(cmd)?;
    let mut dec = match &cmd.dictionary {
        Some(path) => {
            let dict = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            frame::Decoder::with_dict(io::BufReader::new(&mut input), &dict)
        }
        None => frame::Decoder::new(io::BufReader::new(&mut input)),
    };
    let written = io::copy(&mut dec, out).map_err(|e| {
        // prefer the codec's wording when the failure came from the frame layer
        match dec.last_error() {
            Some(codec) => format!("{}: {codec}", describe_input(cmd)),
            None => format!("{}: {e}", describe_input(cmd)),
        }
    })?;
    if dec.info().is_none() {
        return Err(format!("{}: not an LZ4 frame", describe_input(cmd)));
    }
    Ok(written)
}

/// `LZ4F_optimalBSID`: the smallest 64 KiB..4 MiB block that still holds `src_len`, never
/// larger than what the caller asked for.
fn optimal_block_size(requested: usize, src_len: usize) -> usize {
    let mut block = 64 << 10;
    while block < requested {
        if src_len <= block {
            return block;
        }
        block <<= 2;
    }
    requested
}

fn describe_input(cmd: &Cmd) -> String {
    match &cmd.input {
        Some(p) => p.display().to_string(),
        None => "(stdin)".to_string(),
    }
}

fn open_input(cmd: &Cmd) -> Result<Box<dyn Read>, String> {
    match &cmd.input {
        Some(path) if path.as_os_str() != "-" => {
            let f = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(Box::new(f))
        }
        _ => Ok(Box::new(io::stdin())),
    }
}

/// Where the result goes. A file is written to `<name>.tmp` and renamed into place, so a
/// crash never leaves half an output file; `commit` is what makes it visible.
enum Output {
    Stdout(io::Stdout),
    File {
        file: fs::File,
        tmp: PathBuf,
        path: PathBuf,
    },
}

fn open_output(cmd: &Cmd) -> Result<Output, String> {
    let to_stdout = cmd.to_stdout || cmd.output.as_deref().map_or(true, |p| p.as_os_str() == "-");
    if to_stdout {
        return Ok(Output::Stdout(io::stdout()));
    }
    let path = cmd.output.as_ref().expect("output path");
    if path.exists() && !cmd.force {
        return Err(format!(
            "{} already exists; use -f to overwrite",
            path.display()
        ));
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let file = fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    Ok(Output::File {
        file,
        tmp,
        path: path.clone(),
    })
}

impl Output {
    fn commit(self) -> Result<(), String> {
        match self {
            Output::Stdout(mut stdout) => stdout.flush().map_err(|e| format!("stdout: {e}")),
            Output::File { file, tmp, path } => {
                drop(file);
                fs::rename(&tmp, &path).map_err(|e| {
                    let _ = fs::remove_file(&tmp);
                    format!("{}: {e}", path.display())
                })
            }
        }
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Output::Stdout(s) => s.write(buf),
            Output::File { file, .. } => file.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Output::Stdout(s) => s.flush(),
            Output::File { file, .. } => file.flush(),
        }
    }
}

fn remove_source(cmd: &Cmd) {
    if !cmd.remove_source {
        return;
    }
    if let Some(input) = &cmd.input {
        if input.as_os_str() != "-" {
            let _ = fs::remove_file(input);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<Action, String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse(&args)
    }

    fn cmd(args: &[&str]) -> Cmd {
        match parse_str(args) {
            Ok(Action::Run(c)) => c,
            other => panic!("{args:?} parsed to {other:?}"),
        }
    }

    #[test]
    fn defaults_compress_to_the_lz4_suffix() {
        let c = cmd(&["file.txt"]);
        assert_eq!(c.mode, Mode::Compress);
        assert_eq!(c.input.as_deref(), Some(Path::new("file.txt")));
        assert_eq!(c.output.as_deref(), Some(Path::new("file.txt.lz4")));
        assert_eq!(c.opts.level, Level::FAST);
        assert_eq!(c.opts.block_size, 4 << 20);
        assert_eq!(c.opts.workers, Workers::Single);
    }

    #[test]
    fn decompress_strips_the_suffix_and_falls_back_to_out() {
        let c = cmd(&["-d", "file.txt.lz4"]);
        assert_eq!(c.mode, Mode::Decompress);
        assert_eq!(c.output.as_deref(), Some(Path::new("file.txt")));
        // a name without the suffix still decodes, to <name>.out
        let c = cmd(&["-d", "mystery.bin"]);
        assert_eq!(c.output.as_deref(), Some(Path::new("mystery.bin.out")));
    }

    #[test]
    fn explicit_output_and_stdout() {
        let c = cmd(&["in", "out.lz4"]);
        assert_eq!(c.output.as_deref(), Some(Path::new("out.lz4")));
        let c = cmd(&["-c", "in"]);
        assert!(c.to_stdout);
        assert_eq!(c.output, None);
        // "-" as output means stdout, "-" as input means stdin
        let c = cmd(&["in", "-"]);
        assert_eq!(c.input.as_deref(), Some(Path::new("in")));
        assert_eq!(c.output.as_deref(), Some(Path::new("-")));
        let c = cmd(&["-"]);
        assert_eq!(c.input.as_deref(), Some(Path::new("-")));
    }

    #[test]
    fn levels_cover_the_cli_range_and_bundling() {
        for n in 1..=12u8 {
            let arg = format!("-{n}");
            assert_eq!(cmd(&[&arg, "f"]).opts.level, Level::new(n).unwrap(), "-{n}");
        }
        assert_eq!(cmd(&["-12", "f"]).opts.level, Level::new(12).unwrap());
        // bundling: level + force
        let c = cmd(&["-9f", "f"]);
        assert_eq!(c.opts.level, Level::new(9).unwrap());
        assert!(c.force);
        // level + block size + dependency
        let c = cmd(&["-B4D", "f"]);
        assert_eq!(c.opts.block_size, 64 << 10);
        assert_eq!(c.opts.block_mode, BlockMode::Linked);
        assert!(cmd(&["-BX", "f"]).opts.block_checksum);
    }

    #[test]
    fn workers_and_frame_switches() {
        assert_eq!(cmd(&["-T0", "f"]).opts.workers, Workers::Auto);
        assert_eq!(cmd(&["-T4", "f"]).opts.workers, Workers::Fixed(4));
        assert_eq!(cmd(&["--single-thread", "f"]).opts.workers, Workers::Single);
        assert!(!cmd(&["--no-frame-crc", "f"]).opts.content_checksum);
        assert!(cmd(&["--content-size", "f"]).opts.content_size);
        for (flag, size) in [
            ("-B4", 64 << 10),
            ("-B5", 256 << 10),
            ("-B6", 1 << 20),
            ("-B7", 4 << 20),
        ] {
            assert_eq!(cmd(&[flag, "f"]).opts.block_size, size, "{flag}");
        }
    }

    #[test]
    fn modes_and_help() {
        assert_eq!(
            parse_str(&["-t", "f.lz4"]).unwrap(),
            Action::Run(cmd(&["-t", "f.lz4"]))
        );
        assert_eq!(cmd(&["-t", "f.lz4"]).mode, Mode::Test);
        assert_eq!(cmd(&["-d"],).mode, Mode::Decompress);
        assert_eq!(parse_str(&["-h"]).unwrap(), Action::Help);
        assert_eq!(parse_str(&["--help"]).unwrap(), Action::Help);
        assert_eq!(parse_str(&["-V"]).unwrap(), Action::Version);
        // -z cancels an earlier -d, later flags win
        assert_eq!(cmd(&["-d", "-z", "f"]).mode, Mode::Compress);
        assert_eq!(cmd(&["-z", "-d", "f"]).mode, Mode::Decompress);
    }

    #[test]
    fn bad_input_is_rejected_with_a_message() {
        assert!(parse_str(&["-13", "f"]).is_err(), "level 13");
        assert!(parse_str(&["--nope", "f"]).is_err(), "unknown long option");
        assert!(parse_str(&["-Z", "f"]).is_err(), "unknown short option");
        assert!(parse_str(&["-B3", "f"]).is_err(), "block size id 3");
        assert!(parse_str(&["-T", "f"]).is_err(), "-T without a number");
        assert!(parse_str(&["a", "b", "c"]).is_err(), "three files");
    }

    #[test]
    fn block_size_shrinks_to_fit_the_input() {
        assert_eq!(optimal_block_size(4 << 20, 15_000), 64 << 10);
        assert_eq!(optimal_block_size(4 << 20, 64 << 10), 64 << 10);
        assert_eq!(optimal_block_size(4 << 20, (64 << 10) + 1), 256 << 10);
        assert_eq!(optimal_block_size(4 << 20, 3 << 20), 4 << 20);
        assert_eq!(optimal_block_size(4 << 20, 100 << 20), 4 << 20);
        // an explicit smaller request is never enlarged
        assert_eq!(optimal_block_size(64 << 10, 100 << 20), 64 << 10);
    }

    #[test]
    fn dictionary_option_takes_a_file() {
        let c = cmd(&["-D", "dict.bin", "-d", "in.lz4"]);
        assert_eq!(c.dictionary.as_deref(), Some(Path::new("dict.bin")));
        assert_eq!(c.mode, Mode::Decompress);
        assert_eq!(
            cmd(&["--dictionary", "d", "f"]).dictionary.as_deref(),
            Some(Path::new("d"))
        );
        assert!(parse_str(&["-D"]).is_err(), "-D without a file name");
        assert_eq!(cmd(&["f"]).dictionary, None);
    }

    #[test]
    fn console_rules() {
        let c = cmd(&["-c"]);
        // reading a file from a keyboard would look like a hang
        assert!(check_terminals(&c, true, false).is_err());
        assert!(check_terminals(&c, false, false).is_ok());
        // ... unless the user insists
        let c = cmd(&["-c", "-f"]);
        assert!(check_terminals(&c, true, false).is_ok());

        // compressed bytes on a console are refused, decompressed text is allowed
        let c = cmd(&["-c", "in"]);
        assert!(check_terminals(&c, false, true).is_err());
        assert!(check_terminals(&c, false, false).is_ok());
        let c = cmd(&["-d", "-c", "in.lz4"]);
        assert!(check_terminals(&c, false, true).is_ok());
        // a file output never touches the console
        let c = cmd(&["in", "out.lz4"]);
        assert!(check_terminals(&c, false, true).is_ok());
    }

    #[test]
    fn double_dash_ends_the_flags() {
        let c = cmd(&["--", "-weird-name"]);
        assert_eq!(c.input.as_deref(), Some(Path::new("-weird-name")));
        assert_eq!(c.output.as_deref(), Some(Path::new("-weird-name.lz4")));
    }
}
