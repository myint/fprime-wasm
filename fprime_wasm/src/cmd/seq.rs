//! `fprime-wasm seq`: compile `.seq` command sequences to modules. Loading one on the
//! interpreter is `verify`'s job.

use super::cli::Seq;
use super::dictionary;
use anyhow::{Context, Result, anyhow, bail};
use fprime_dictionary::Dictionary;
use fprime_test::project::Project;
use fprime_wasm::scaffold::DICTIONARY_DIR;
use fprime_wasm::seq;
use std::path::{Path, PathBuf};

/// Returns whether every sequence compiled.
pub fn run(args: &Seq) -> Result<bool> {
    if args.output.is_some() && args.sequences.len() > 1 {
        bail!(
            "--output names one module, but {} sequences were given. Drop it to write each \
             module next to its source",
            args.sequences.len()
        );
    }

    let path = match &args.dictionary {
        Some(path) => path.clone(),
        None => project_dictionary()?,
    };
    let dictionary = fprime_dictionary::try_parse(&path).map_err(|err| anyhow!("{err}"))?;

    let jobs: Vec<(&PathBuf, PathBuf)> = args
        .sequences
        .iter()
        .map(|source| {
            let output = args
                .output
                .clone()
                .unwrap_or_else(|| source.with_extension("wasm"));
            (source, output)
        })
        .collect();
    check_outputs(&jobs)?;

    let mut all = true;
    for (source, output) in &jobs {
        all &= compile(source, output, &dictionary)?;
    }
    Ok(all)
}

/// Refuse, before anything is written, a module that would overwrite a source (its own or
/// another's) or another sequence's module.
fn check_outputs(jobs: &[(&PathBuf, PathBuf)]) -> Result<()> {
    let sources: Vec<PathBuf> = jobs.iter().map(|(source, _)| identity(source)).collect();
    let mut written: Vec<(PathBuf, &Path)> = vec![];
    for (source, output) in jobs {
        let target = identity(output);
        if sources.contains(&target) {
            bail!(
                "{} would be overwritten by the module for {}; name the output with --output",
                output.display(),
                source.display()
            );
        }
        if let Some((_, earlier)) = written.iter().find(|(path, _)| *path == target) {
            bail!(
                "{} and {} would both be written to {}; compile them separately with --output",
                earlier.display(),
                source.display(),
                output.display()
            );
        }
        written.push((target, source));
    }
    Ok(())
}

/// `path` resolved enough to tell whether two spellings name the same file: absolute, with its
/// directory's links and `..`s resolved when that directory exists.
fn identity(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(directory), Some(name)) => directory
            .canonicalize()
            .map(|directory| directory.join(name))
            .unwrap_or(absolute),
        _ => absolute,
    }
}

/// Diagnostics go to stderr as `file:line:column: level: message`. Nothing is written for a
/// sequence with errors.
fn compile(source: &Path, output: &Path, dictionary: &Dictionary) -> Result<bool> {
    let text = std::fs::read_to_string(source)
        .with_context(|| format!("could not read {}", source.display()))?;
    let label = source.display().to_string();

    match seq::compile(&text, dictionary) {
        Ok(compiled) => {
            for warning in &compiled.warnings {
                eprintln!("{}", warning.render(&label));
            }
            std::fs::write(output, &compiled.wasm)
                .with_context(|| format!("could not write {}", output.display()))?;
            println!(
                "{label} -> {} ({} bytes)",
                output.display(),
                compiled.wasm.len()
            );
            Ok(true)
        }
        Err(diagnostics) => {
            for diagnostic in &diagnostics {
                eprintln!("{}", diagnostic.render(&label));
            }
            let errors = diagnostics.iter().filter(|d| d.is_error()).count();
            eprintln!(
                "{label}: {errors} error{}, nothing written",
                if errors == 1 { "" } else { "s" }
            );
            Ok(false)
        }
    }
}

/// The dictionary `init` copied into the project the current directory is in.
fn project_dictionary() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let project = Project::find(&cwd).map_err(|err| {
        anyhow!("{err:#}. Alternatively, pass --dictionary <path> to the deployment's dictionary")
    })?;
    let relative = dictionary::existing(project.root()).with_context(|| {
        format!(
            "no dictionary under {}. Pass --dictionary <path> to the deployment's dictionary",
            project.root().join(DICTIONARY_DIR).display()
        )
    })?;
    Ok(project.root().join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DICTIONARY: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../fprime_dictionary/src/test/RefTopologyDictionary.json"
    );

    /// A fresh directory under the system temp dir, named for the test.
    fn scratch(test: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("fprime-wasm-seq-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn args(sequences: Vec<PathBuf>, output: Option<PathBuf>) -> Seq {
        Seq {
            sequences,
            dictionary: Some(PathBuf::from(DICTIONARY)),
            output,
        }
    }

    #[test]
    fn writes_each_module_next_to_its_source() {
        let directory = scratch("next-to");
        let good = directory.join("good.seq");
        std::fs::write(&good, "R00:00:00 CMD_NO_OP\n").unwrap();

        assert!(run(&args(vec![good], None)).unwrap());
        let module = std::fs::read(directory.join("good.wasm")).unwrap();
        assert_eq!(&module[..4], b"\0asm");
    }

    #[test]
    fn writes_nothing_for_a_sequence_with_errors() {
        let directory = scratch("errors");
        let good = directory.join("good.seq");
        let bad = directory.join("bad.seq");
        std::fs::write(&good, "R00:00:00 CMD_NO_OP\n").unwrap();
        std::fs::write(&bad, "R00:00:00 NO_SUCH_COMMAND\n").unwrap();

        // The good one is still compiled; the run as a whole fails.
        assert!(!run(&args(vec![bad, good], None)).unwrap());
        assert!(!directory.join("bad.wasm").exists());
        assert!(directory.join("good.wasm").exists());
    }

    #[test]
    fn output_names_a_single_module() {
        let directory = scratch("output");
        let source = directory.join("in.seq");
        let output = directory.join("elsewhere.wasm");
        std::fs::write(&source, "R00:00:00 CMD_NO_OP\n").unwrap();

        assert!(run(&args(vec![source.clone()], Some(output.clone()))).unwrap());
        assert!(output.exists());

        let two = args(vec![source.clone(), source], Some(output));
        assert!(run(&two).is_err());
    }

    #[test]
    fn refuses_two_sequences_one_module() {
        let directory = scratch("collide");
        let seq = directory.join("a.seq");
        let txt = directory.join("a.txt");
        std::fs::write(&seq, "R00:00:00 CMD_NO_OP\n").unwrap();
        std::fs::write(&txt, "R00:00:00 CMD_NO_OP\n").unwrap();

        let err = run(&args(vec![seq, txt], None)).unwrap_err();
        assert!(err.to_string().contains("would both be written"), "{err}");
        assert!(!directory.join("a.wasm").exists(), "nothing is written");
    }

    #[test]
    fn refuses_to_overwrite_a_source_however_it_is_spelled() {
        let directory = scratch("spelling");
        let source = directory.join("in.seq");
        std::fs::write(&source, "R00:00:00 CMD_NO_OP\n").unwrap();
        let other = directory.join("sub/../in.seq");
        std::fs::create_dir_all(directory.join("sub")).unwrap();

        let err = run(&args(vec![source.clone()], Some(other))).unwrap_err();
        assert!(err.to_string().contains("would be overwritten"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "R00:00:00 CMD_NO_OP\n"
        );
    }

    #[test]
    fn refuses_to_overwrite_its_source() {
        let directory = scratch("overwrite");
        let source = directory.join("already.wasm");
        std::fs::write(&source, "R00:00:00 CMD_NO_OP\n").unwrap();

        let err = run(&args(vec![source.clone()], None)).unwrap_err();
        assert!(err.to_string().contains("would be overwritten"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "R00:00:00 CMD_NO_OP\n"
        );
    }
}
