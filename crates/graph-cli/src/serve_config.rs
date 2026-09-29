//! `serve --config <file.toml>` (ADR 0004 Q2, epic story 25 AC 4, issue
//! #106): `serve`'s settings from a TOML file.
//!
//! Every `serve` flag has a key of the same name, kebab-case or
//! snake_case (`data-dir` or `data_dir`), plus the global `db` and
//! `cache-bytes` that `serve` uses. Values are what the flag takes: a
//! string, a number, `true`/`false` for a switch (false: not given), an
//! array for a list (`peers = ["a:7000", "b:7000"]`, passed as one
//! `--peers=<item>` per item, like the repeated flag). Precedence, highest
//! first: a flag on the command line, its environment variable (the few
//! flags that have one, e.g. `MEMORY_GRAPH_LOG`), the file, the built-in
//! default. Relative paths in the file are relative to the working
//! directory, as on the command line. An unknown key is an error, so a typo
//! never silently falls back to a default.
//!
//! The file is applied by rewriting the argument list before clap parses
//! it: each key whose flag was not given (on the command line or by its
//! environment variable) becomes `--key=value` at the end, and clap then
//! validates the whole as if typed. So the file and the flags cannot
//! diverge: one parser, one set of rules (`requires`, `conflicts_with`,
//! value parsers) for both.
use anyhow::{bail, Context, Result};
use clap::parser::ValueSource;
use clap::Command;
use std::ffi::OsString;

/// The id of the `--config` argument on `serve`.
pub const CONFIG_ARG: &str = "config";

/// The global flags that apply to `serve` and so may be set in its file.
const GLOBALS: [&str; 2] = ["db", "cache-bytes"];

/// `args` with the settings of `serve --config <file>` appended (see the
/// module docs); unchanged for any other command, without `--config`, or
/// when clap cannot read `args` even leniently (it then reports that
/// itself). The command line is read with `ignore_errors`: it may be
/// incomplete on its own (`--bootstrap` requires `--data-dir`, which the
/// file gives), and the full parse after the merge checks the whole.
pub fn apply(cmd: &Command, args: Vec<OsString>) -> Result<Vec<OsString>> {
    let Ok(top) = cmd.clone().ignore_errors(true).try_get_matches_from(&args) else {
        return Ok(args);
    };
    let Some(("serve", sm)) = top.subcommand() else {
        return Ok(args);
    };
    let Some(path) = sm.get_raw(CONFIG_ARG).and_then(|mut v| v.next()) else {
        return Ok(args);
    };
    let path = std::path::PathBuf::from(path);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("--config `{}`: cannot read it", path.display()))?;
    let extra =
        settings(cmd, sm, &text).with_context(|| format!("--config `{}`", path.display()))?;
    let mut args = args;
    args.extend(extra);
    Ok(args)
}

/// The arguments the TOML `text` adds, given what `sm` (the `serve`
/// matches of the command line) already sets.
fn settings(cmd: &Command, sm: &clap::ArgMatches, text: &str) -> Result<Vec<OsString>> {
    let table: toml::Table = text.parse().context("not valid TOML")?;
    let serve = cmd
        .find_subcommand("serve")
        .expect("the CLI has a serve command");
    let mut out = Vec::new();
    for (key, value) in &table {
        let long = key.replace('_', "-");
        let arg = serve
            .get_arguments()
            .chain(
                cmd.get_arguments()
                    .filter(|a| a.get_long().is_some_and(|l| GLOBALS.contains(&l))),
            )
            .find(|a| a.get_long() == Some(long.as_str()) && a.get_id() != CONFIG_ARG);
        let Some(arg) = arg else {
            let mut known: Vec<&str> = serve
                .get_arguments()
                .filter_map(|a| a.get_long())
                .filter(|l| *l != CONFIG_ARG && *l != "help")
                .chain(GLOBALS)
                .collect();
            known.sort_unstable();
            bail!(
                "unknown key `{key}`; the keys are serve's flags: {}",
                known.join(", ")
            );
        };
        let id = arg.get_id().as_str();
        if matches!(
            sm.value_source(id),
            Some(ValueSource::CommandLine | ValueSource::EnvVariable)
        ) {
            continue; // The flag (or its environment variable) wins.
        }
        let flag = format!("--{long}");
        if !arg.get_action().takes_values() {
            match value {
                toml::Value::Boolean(true) => out.push(flag.into()),
                toml::Value::Boolean(false) => {}
                other => bail!(
                    "`{key}` is a switch: give true or false, not {}",
                    other.type_str()
                ),
            }
            continue;
        }
        let scalar = |v: &toml::Value| -> Result<String> {
            Ok(match v {
                toml::Value::String(s) => s.clone(),
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(f) => f.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                other => bail!(
                    "`{key}` takes a value like --{long} does, not {}",
                    other.type_str()
                ),
            })
        };
        match value {
            toml::Value::Array(items) => {
                let items: Vec<String> = items.iter().map(scalar).collect::<Result<_>>()?;
                // One `--flag=v` per item, as a repeated flag is typed (no
                // joining, so an item is never re-split or merged).
                let list = arg.get_value_delimiter().is_some()
                    || matches!(arg.get_action(), clap::ArgAction::Append);
                if !list {
                    bail!("`{key}` takes one value, not a list");
                }
                out.extend(items.iter().map(|i| OsString::from(format!("{flag}={i}"))));
            }
            v => out.push(format!("{flag}={}", scalar(v)?).into()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction};

    fn cmd() -> Command {
        Command::new("mg")
            .arg(Arg::new("db").long("db").global(true))
            .arg(Arg::new("server").long("server").global(true))
            .subcommand(
                Command::new("serve")
                    .arg(Arg::new("config").long("config"))
                    .arg(
                        Arg::new("listen")
                            .long("listen")
                            .default_value("127.0.0.1:7000"),
                    )
                    .arg(Arg::new("data_dir").long("data-dir"))
                    .arg(
                        Arg::new("bootstrap")
                            .long("bootstrap")
                            .action(ArgAction::SetTrue),
                    )
                    .arg(
                        Arg::new("peers")
                            .long("peers")
                            .value_delimiter(',')
                            .action(ArgAction::Append),
                    )
                    .arg(
                        Arg::new("log_level")
                            .long("log-level")
                            .env("MG_TEST_LOG_LEVEL"),
                    ),
            )
            .subcommand(Command::new("describe"))
    }

    fn with_file(text: &str, args: &[&str]) -> Result<Vec<String>> {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("serve.toml");
        std::fs::write(&f, text).unwrap();
        let mut a: Vec<OsString> = args.iter().map(OsString::from).collect();
        a.extend(["--config".into(), f.clone().into_os_string()]);
        let out = apply(&cmd(), a)?;
        // Drop the path (it differs per run).
        Ok(out
            .into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.ends_with("serve.toml"))
            .collect())
    }

    #[test]
    fn keys_become_flags_that_the_command_line_overrides() {
        let out = with_file(
            "listen = \"0.0.0.0:7000\"\ndata_dir = \"/data\"\nbootstrap = true\n\
             peers = [\"a:1\", \"b:2\"]\ndb = \"x.redb\"\n",
            &["mg", "serve", "--data-dir", "/other"],
        )
        .unwrap();
        assert_eq!(
            out,
            [
                "mg",
                "serve",
                "--data-dir",
                "/other",
                "--config",
                "--bootstrap",
                "--db=x.redb",
                "--listen=0.0.0.0:7000",
                "--peers=a:1",
                "--peers=b:2"
            ]
        );
    }

    #[test]
    fn false_switches_add_nothing_and_other_commands_are_untouched() {
        let out = with_file("bootstrap = false\n", &["mg", "serve"]).unwrap();
        assert_eq!(out, ["mg", "serve", "--config"]);
        let a: Vec<OsString> = ["mg", "describe"].iter().map(OsString::from).collect();
        assert_eq!(apply(&cmd(), a.clone()).unwrap(), a);
    }

    #[test]
    fn mistakes_are_errors_that_name_the_key() {
        let e = with_file("lisen = \"x\"\n", &["mg", "serve"]).unwrap_err();
        assert!(format!("{e:#}").contains("unknown key `lisen`"), "{e:#}");
        assert!(format!("{e:#}").contains("listen"), "{e:#}");
        let e = with_file("bootstrap = \"yes\"\n", &["mg", "serve"]).unwrap_err();
        assert!(format!("{e:#}").contains("is a switch"), "{e:#}");
        let e = with_file("listen = [\"a\"]\n", &["mg", "serve"]).unwrap_err();
        assert!(format!("{e:#}").contains("not a list"), "{e:#}");
        let e = with_file("listen = \n", &["mg", "serve"]).unwrap_err();
        assert!(format!("{e:#}").contains("not valid TOML"), "{e:#}");
        let e = with_file("server = \"a:1\"\n", &["mg", "serve"]).unwrap_err();
        assert!(format!("{e:#}").contains("unknown key `server`"), "{e:#}");
        let e = with_file("config = \"other.toml\"\n", &["mg", "serve"]).unwrap_err();
        assert!(format!("{e:#}").contains("unknown key `config`"), "{e:#}");
        let a: Vec<OsString> = ["mg", "serve", "--config", "/no/such/file.toml"]
            .iter()
            .map(OsString::from)
            .collect();
        let e = apply(&cmd(), a).unwrap_err();
        assert!(format!("{e:#}").contains("cannot read it"), "{e:#}");
    }
}
