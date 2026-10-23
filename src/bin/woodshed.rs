//! `woodshed` - maintain `native/versions.toml`.

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
woodshed - maintain native/versions.toml (checksums are computed for you)

USAGE:
  woodshed add <name> --version <v> --url <url> [--marker <path>]... [--file <name>] [--manifest <path>]
  woodshed bump <name> <new-version> [--url <url>] [--manifest <path>]
  woodshed verify [--manifest <path>]
  woodshed bundle --output <new-directory> -- <cargo options: --manifest-path, --bin, --release, ...>

`{version}` and `{name}` in a URL are substituted, so a template URL only needs
the version changed on a bump. Default manifest: native/versions.toml
";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Splits `--flag value` pairs from positional arguments.
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, String)>,
}

impl Args {
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let (mut positional, mut flags) = (Vec::new(), Vec::new());
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            if let Some(flag) = arg.strip_prefix("--") {
                let v = it.next().ok_or(format!("--{flag} needs a value"))?;
                flags.push((flag.to_string(), v));
            } else {
                positional.push(arg);
            }
        }
        Ok(Self { positional, flags })
    }

    fn one(&self, flag: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| f == flag)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, flag: &str) -> Vec<String> {
        self.flags
            .iter()
            .filter(|(f, _)| f == flag)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

fn run(raw: Vec<String>) -> Result<(), String> {
    if raw.first().is_some_and(|command| command == "bundle") {
        if raw.len() < 4 || raw[1] != "--output" || raw[3] != "--" {
            return Err(
                "usage: woodshed bundle --output <new-directory> -- <cargo target/build options>"
                    .into(),
            );
        }
        let arguments = raw[4..].iter().map(Into::into).collect::<Vec<_>>();
        let executable = woodshed::bundle_cargo(std::path::Path::new(&raw[2]), &arguments)
            .map_err(|error| error.to_string())?;
        println!("Bundled {}", executable.display());
        return Ok(());
    }
    let Some((command, rest)) = raw.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    let args = Args::parse(rest.to_vec())?;
    let manifest = PathBuf::from(args.one("manifest").unwrap_or("native/versions.toml"));
    let err = |e: woodshed::Error| e.to_string();

    match command.as_str() {
        "add" => {
            let name = args.positional.first().ok_or("add needs a name")?;
            let version = args.one("version").ok_or("add needs --version")?;
            let url = args.one("url").ok_or("add needs --url")?;
            let markers = args.all("marker");
            let sha = woodshed::pin::add(&manifest, name, version, url, &markers, args.one("file"))
                .map_err(err)?;
            println!("added [{name}] {version}  sha256 = {sha}");
            if markers.is_empty() {
                println!(
                    "note: no markers set; add `markers = [..]` (a build file and a core source \
                     file) so interrupted extractions are detected"
                );
            }
        }
        "bump" => {
            let [name, version] = args.positional.as_slice() else {
                return Err("usage: woodshed bump <name> <new-version>".into());
            };
            let sha =
                woodshed::pin::bump(&manifest, name, version, args.one("url")).map_err(err)?;
            println!("bumped [{name}] to {version}  sha256 = {sha}");
        }
        "verify" => {
            let mut bad = 0;
            for (name, outcome) in woodshed::pin::verify(&manifest).map_err(err)? {
                match outcome {
                    Ok(()) => println!("ok       {name}"),
                    Err(e) => {
                        bad += 1;
                        println!("MISMATCH {name}: {e}");
                    }
                }
            }
            if bad > 0 {
                return Err(format!("{bad} pin(s) failed verification"));
            }
        }
        _ => print!("{USAGE}"),
    }
    Ok(())
}
