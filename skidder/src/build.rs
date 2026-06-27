use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, ensure, Context, Result};
use sha1::{Digest, Sha1};
use tempfile::TempDir;
use walkdir::WalkDir;

use crate::{decompress, Metadata, LIB_EXTENSION};

type Checksum = [u8; 20];
fn is_fresh(grammar_dir: &Path, force: bool) -> Result<(Checksum, bool)> {
    let src_dir = grammar_dir.join("src");
    let cookie = grammar_dir.join(".BUILD_COOKIE");
    let mut hasher = Sha1::new();
    for file in WalkDir::new(src_dir) {
        let file = file?;
        let file_type = file.file_type();
        // Hash any .c, .cc or .h file
        if !file_type.is_file() {
            continue;
        }
        let file_name = file.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some((_, extension)) = file_name.rsplit_once('.') else {
            continue;
        };
        if !matches!(extension, "h" | "c" | "cc") {
            continue;
        }
        let path = file.path();

        hasher.update(file_name.as_bytes());
        hasher.update([0, 0, 0, 0]);
        File::open(path)
            .and_then(|mut file| io::copy(&mut file, &mut hasher))
            .with_context(|| format!("failed to read {}", path.display()))?;
        hasher.update([0, 0, 0, 0]);
    }

    let checksum = hasher.finalize();
    if force {
        return Ok((checksum.into(), false));
    }
    let Ok(prev_checksum) = fs::read(cookie) else {
        return Ok((checksum.into(), false));
    };
    Ok((checksum.into(), prev_checksum == checksum[..]))
}

#[cfg(not(windows))]
const SCANNER_OBJECT: &str = "scanner.o";
#[cfg(windows)]
const SCANNER_OBJECT: &str = "scanner.obj";
const BUILD_TARGET: &str = env!("BUILD_TARGET");
static CPP_COMPILER: OnceLock<cc::Tool> = OnceLock::new();
static C_COMPILER: OnceLock<cc::Tool> = OnceLock::new();

enum CompilerCommand {
    Build,
    BuildAndLink { obj_files: Vec<&'static str> },
}
impl CompilerCommand {
    pub fn setup(self, build_dir: &Path, src_dir: &Path, file: &Path, out_file: &str) -> Command {
        let cpp = file.extension().is_some_and(|ext| ext == "cc");
        let compiler = if cpp {
            CPP_COMPILER.get_or_init(|| {
                cc::Build::new()
                    .cpp(true)
                    .opt_level(3)
                    .std("c++14")
                    .debug(false)
                    .cargo_metadata(false)
                    .host(BUILD_TARGET)
                    .target(BUILD_TARGET)
                    .get_compiler()
            })
        } else {
            C_COMPILER.get_or_init(|| {
                cc::Build::new()
                    // Note that we use a C++ compiler but force C mode below
                    // with "-xc". This is important for compilation of grammars
                    // that have C++ scanners. If we used `cpp(false)` then the
                    // scanner might miss symbols from the C++ standard library.
                    .cpp(true)
                    .debug(false)
                    .opt_level(3)
                    .std("c11")
                    .cargo_metadata(false)
                    .host(BUILD_TARGET)
                    .target(BUILD_TARGET)
                    .get_compiler()
            })
        };
        let mut cmd = compiler.to_command();
        cmd.current_dir(build_dir);
        if compiler.is_like_msvc() {
            cmd.args(["/nologo", "/LD", "/utf-8", "/I"]).arg(src_dir);
            match self {
                CompilerCommand::Build => {
                    cmd.arg(format!("/Fo{out_file}")).arg("/c").arg(file);
                }
                CompilerCommand::BuildAndLink { obj_files } => {
                    cmd.args(obj_files)
                        .arg(file)
                        .arg("/link")
                        .arg(format!("/out:{out_file}"));
                }
            }
        } else {
            #[cfg(not(windows))]
            cmd.arg("-fPIC");
            cmd.args(["-shared", "-fno-exceptions", "-o", out_file, "-I"])
                .arg(src_dir);
            if cfg!(all(
                unix,
                not(any(target_os = "macos", target_os = "illumos"))
            )) {
                cmd.arg("-Wl,-z,relro,-z,now");
            }
            match self {
                CompilerCommand::Build => {
                    cmd.arg("-c");
                }
                CompilerCommand::BuildAndLink { obj_files } => {
                    cmd.args(obj_files);
                }
            }
            if !cpp {
                cmd.arg("-xc");
            }
            cmd.arg(file);
        };
        cmd
    }
}

pub fn build_grammar(grammar_name: &str, grammar_dir: &Path, force: bool) -> Result<()> {
    let src_dir = grammar_dir.join("src");
    let mut parser = src_dir.join("parser.c");
    ensure!(
        parser.exists(),
        "failed to compile {grammar_name}: {} not found!",
        parser.display()
    );
    let (hash, fresh) = is_fresh(grammar_dir, force)?;
    if fresh {
        return Ok(());
    }
    let build_dir = TempDir::new().context("failed to create temporary build directory")?;
    let metadata = Metadata::read(&grammar_dir.join("metadata.json"))
        .with_context(|| format!("failed to read metadata for {grammar_name}"))?;
    let Some(parser_definition) = metadata.parser_definition() else {
        bail!("source directories with parser.c files must have parser definition metadata");
    };
    if parser_definition.compressed {
        let decompressed_parser = build_dir.path().join(format!("{grammar_name}.c"));
        let mut dst = File::create(&decompressed_parser).with_context(|| {
            format!(
                "failed to create parser.c file in temporary build directory {}",
                build_dir.path().display()
            )
        })?;
        File::open(&parser)
            .map_err(anyhow::Error::from)
            .and_then(|mut reader| decompress(&mut reader, &mut dst))
            .with_context(|| {
                format!("failed to decompress parser {}", build_dir.path().display())
            })?;
        parser = decompressed_parser;
    }
    let mut commands = Vec::new();
    let mut obj_files = Vec::new();
    if src_dir.join("scanner.c").exists() {
        let scanner_cmd = CompilerCommand::Build.setup(
            build_dir.path(),
            &src_dir,
            &src_dir.join("scanner.c"),
            SCANNER_OBJECT,
        );
        obj_files.push(SCANNER_OBJECT);
        commands.push(scanner_cmd)
    } else if src_dir.join("scanner.cc").exists() {
        let scanner_cmd = CompilerCommand::Build.setup(
            build_dir.path(),
            &src_dir,
            &src_dir.join("scanner.cc"),
            SCANNER_OBJECT,
        );
        obj_files.push(SCANNER_OBJECT);
        commands.push(scanner_cmd)
    }
    let lib_name = format!("{grammar_name}.{LIB_EXTENSION}");
    let parser_cmd = CompilerCommand::BuildAndLink { obj_files }.setup(
        build_dir.path(),
        &src_dir,
        &parser,
        &lib_name,
    );
    commands.push(parser_cmd);

    for mut cmd in commands {
        let output = cmd.output().context("Failed to execute compiler")?;
        if !output.status.success() {
            bail!(
                "Parser compilation failed.\nStdout: {}\nStderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    let from = build_dir.path().join(lib_name);
    let to = grammar_dir.join(grammar_name).with_extension(LIB_EXTENSION);
    fs::copy(&from, &to).with_context(|| {
        format!(
            "failed to copy compiled library from {} to {}",
            from.display(),
            to.display()
        )
    })?;
    let _ = fs::write(grammar_dir.join(".BUILD_COOKIE"), hash);
    Ok(())
}
