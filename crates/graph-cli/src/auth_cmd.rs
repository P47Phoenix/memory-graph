//! `memory-graph auth new-token` and `auth new-secret` (ADR 0012 D2/D3):
//! create a credential, show it once, and record only its SHA-256.
//!
//! The token is printed once (stdout, or `--token-out`, a new file with
//! mode 0600 on Unix). The token file stanza (id, role, digest) goes to
//! stderr, or is appended to `--append <file>` (created with mode 0600). No
//! message, stanza or error ever contains the token.
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use zeroize::Zeroizing;

/// What `new-token` made.
pub struct NewToken {
    /// The token itself (zeroized on drop).
    pub token: Zeroizing<String>,
    /// The `[[token]]` stanza for the server's token file.
    pub stanza: String,
}

/// A token id: free text for logs, restricted so the stanza stays one
/// plain TOML string.
fn check_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@:/+".contains(&b))
    {
        bail!(
            "--id `{id}`: use 1 to 128 letters, digits and . _ - @ : / + (it names the token in \
             the server's logs)"
        );
    }
    Ok(())
}

/// Open `path` for writing with mode 0600 on Unix: a new file only
/// (`append` false), or appending to an existing one or a new one.
pub fn open_private(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    if append {
        o.append(true).create(true);
    } else {
        o.write(true).create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Create a token with this id and role (`read` or `write`) and an
/// optional expiry (RFC 3339). With `append`, the stanza is appended to
/// that token file after checking that it stays valid (no duplicate id).
pub fn new_token(
    id: &str,
    role: &str,
    expires: Option<&str>,
    append: Option<&Path>,
) -> Result<NewToken> {
    check_id(id)?;
    if graph_server::Role::parse(role).is_none() {
        bail!("--role `{role}`: expected read or write");
    }
    if let Some(e) = expires {
        if graph_server::auth::parse_rfc3339(e).is_none() {
            bail!(
                "--expires `{e}`: not an RFC 3339 date-time with an offset (2027-01-01T00:00:00Z)"
            );
        }
    }
    let token = graph_server::auth::new_credential(graph_proto::auth::TOKEN_PREFIX);
    let mut stanza = format!(
        "[[token]]\nid = \"{id}\"\nrole = \"{role}\"\nsha256 = \"{}\"\n",
        graph_proto::auth::digest_hex(&token)
    );
    if let Some(e) = expires {
        stanza.push_str(&format!("expires = \"{e}\"\n"));
    }
    if let Some(path) = append {
        let existing = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(e).with_context(|| format!("--append `{}`", path.display()));
            }
        };
        let sep = if existing.is_empty() || existing.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let combined = format!("{existing}{sep}{stanza}");
        if let Err(e) = graph_server::auth::parse_token_file(&combined) {
            bail!(
                "--append `{}`: the file would not be a valid token file ({e}); nothing was \
                 written",
                path.display()
            );
        }
        let mut f =
            open_private(path, true).with_context(|| format!("--append `{}`", path.display()))?;
        let text = if existing.is_empty() {
            stanza.clone()
        } else {
            format!("{sep}{stanza}")
        };
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .with_context(|| format!("--append `{}`", path.display()))?;
    }
    Ok(NewToken { token, stanza })
}

/// Write `secret` (one line) to a new file with mode 0600.
pub fn write_secret_file(path: &Path, secret: &str) -> Result<()> {
    let mut f = open_private(path, false).with_context(|| {
        format!(
            "`{}` (it must not exist: a credential file is never overwritten)",
            path.display()
        )
    })?;
    f.write_all(format!("{secret}\n").as_bytes())
        .and_then(|()| f.sync_all())
        .with_context(|| format!("writing `{}`", path.display()))
}

/// What `auth check` found.
#[derive(Debug, Default)]
pub struct CheckReport {
    /// Lines for stdout (the summary, and the `--explain` decision).
    pub lines: Vec<String>,
    /// Warnings (unknown principals, `action in []`, forbid-only, NFC).
    pub warnings: Vec<String>,
    /// Why the pair is refused (the line and the `@id`), if it is.
    pub error: Option<String>,
}

/// `memory-graph auth check` (ADR 0013 D9): validate a token file and a
/// policy file as `serve` would load the pair, and optionally decide one
/// request. An unreadable file is an `Err`; an invalid pair is a report
/// with `error` set.
pub fn check(
    tokens: &Path,
    policies: &Path,
    builtins: bool,
    explain: Option<&[String]>,
) -> Result<CheckReport> {
    let mut r = CheckReport::default();
    let ttext = std::fs::read_to_string(tokens)
        .with_context(|| format!("cannot read `{}`", tokens.display()))?;
    let pbytes =
        std::fs::read(policies).with_context(|| format!("cannot read `{}`", policies.display()))?;
    let set = match graph_server::auth::parse_token_file(&ttext) {
        Ok(s) => s,
        Err(e) => {
            r.error = Some(format!("`{}`: {e}", tokens.display()));
            return Ok(r);
        }
    };
    let (state, warnings) = match graph_server::auth::pair_from(set, &pbytes, builtins) {
        Ok(x) => x,
        Err(e) => {
            r.error = Some(format!("`{}`: {e}", policies.display()));
            return Ok(r);
        }
    };
    r.warnings = warnings;
    let pset = state.policies.as_ref().expect("a pair has policies");
    r.lines.push(format!(
        "ok: {} policies ({} built-in), {} tokens; policy_hash {}",
        pset.len(),
        if builtins { 2 } else { 0 },
        state.tokens.entries.len(),
        state.policy_hash
    ));
    if let Some([id, action, resource]) = explain {
        let Some(a) = graph_authz::Action::parse(action) else {
            bail!("unknown action `{action}` (read, search, index, prune, delete, snapshot, status, admin)");
        };
        let ent = graph_authz::parse_resource(resource)
            .map_err(|e| anyhow::anyhow!("--explain resource: {}", e.msg))?;
        let res = ent.as_resource().expect("a resource entity");
        let Some(e) = state.tokens.entries.iter().find(|e| &*e.id == id.as_str()) else {
            r.lines.push(format!(
                "deny: token `{id}` is not in the token file (it is refused before any policy)"
            ));
            return Ok(r);
        };
        let p = graph_authz::Principal {
            token: &e.id,
            groups: &e.groups,
            role: match e.role {
                graph_server::auth::Role::Read => graph_authz::Role::Read,
                graph_server::auth::Role::Write => graph_authz::Role::Write,
            },
        };
        let d = pset.decide(&p, a, &res);
        let why = if p.role.ceiling() & a.bit() == 0 {
            format!(" (above the `{}` role's ceiling)", e.role.as_str())
        } else if d.matched.is_empty() {
            " (no policy matched: default deny)".to_string()
        } else {
            format!(" (matched: {})", d.matched.join(", "))
        };
        r.lines.push(format!(
            "{}: Token::\"{id}\" {} {res}{why}",
            if d.allowed { "allow" } else { "deny" },
            a.name()
        ));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_builds_a_valid_file_and_never_writes_the_token() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tokens.toml");
        let a = new_token("ci", "write", None, Some(&p)).unwrap();
        let b = new_token("bot", "read", Some("2030-01-01T00:00:00Z"), Some(&p)).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains(&a.token[4..]) && !text.contains(&b.token[4..]));
        assert!(!a.stanza.contains(&a.token[4..]));
        let set = graph_server::auth::parse_token_file(&text).unwrap();
        assert_eq!(set.entries.len(), 2);
        let e = new_token("ci", "read", None, Some(&p))
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("duplicate"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text, "unchanged");
        assert!(new_token("a b", "read", None, None).is_err());
        assert!(new_token("a", "admin", None, None).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
            let out = d.path().join("t");
            write_secret_file(&out, &a.token).unwrap();
            let mode = std::fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        let out = d.path().join("t2");
        write_secret_file(&out, &a.token).unwrap();
        assert!(
            write_secret_file(&out, &a.token).is_err(),
            "never overwrites"
        );
    }

    /// Story 75: the worked examples of `docs/security.md` validate, and every
    /// decision in `docs/examples/policies/expected.txt` matches `--explain`.
    #[test]
    fn worked_examples() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/policies");
        let (toks, pols) = (dir.join("tokens.toml"), dir.join("policies.cedar"));
        let r = check(&toks, &pols, false, None).unwrap();
        assert!(r.error.is_none(), "{:?}", r.error);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let expected = std::fs::read_to_string(dir.join("expected.txt")).unwrap();
        let mut n = 0;
        for line in expected
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let (req, want) = line.split_once(" => ").unwrap();
            let parts: Vec<String> = req.splitn(3, ' ').map(str::to_string).collect();
            let mut want = want.split(' ');
            let decision = want.next().unwrap();
            let r = check(&toks, &pols, false, Some(&parts)).unwrap();
            let got = &r.lines[1];
            assert!(got.starts_with(&format!("{decision}:")), "{line}: {got}");
            for id in want.filter(|w| *w != "-") {
                assert!(got.contains(id), "{line}: {got}");
            }
            n += 1;
        }
        assert!(n >= 10);
    }

    #[test]
    fn auth_check_validates_a_pair_and_explains() {
        let d = tempfile::tempdir().unwrap();
        let toks = d.path().join("tokens.toml");
        let pols = d.path().join("policies.cedar");
        let t = new_token("ci", "write", None, Some(&toks)).unwrap();
        drop(t);
        std::fs::write(
            &pols,
            "@id(\"web\")\npermit(principal == Token::\"ci\", action == Action::\"index\", resource == Repo::\"acme/web\");\n\
             @id(\"ghost\")\npermit(principal == Token::\"gone\", action, resource);\n",
        )
        .unwrap();
        let ex = |a: &str, r: &str| -> Vec<String> { vec!["ci".into(), a.into(), r.into()] };
        let r = check(
            &toks,
            &pols,
            false,
            Some(&ex("index", "Repo::\"acme/web\"")),
        )
        .unwrap();
        assert!(r.error.is_none(), "{:?}", r.error);
        assert!(r.lines[0].starts_with("ok: 2 policies"), "{:?}", r.lines);
        assert!(
            r.lines[1].starts_with("allow:") && r.lines[1].contains("matched: web"),
            "{:?}",
            r.lines
        );
        assert!(
            r.warnings.iter().any(|w| w.contains("gone")),
            "{:?}",
            r.warnings
        );
        let r = check(
            &toks,
            &pols,
            false,
            Some(&ex("index", "Repo::\"acme/api\"")),
        )
        .unwrap();
        assert!(r.lines[1].starts_with("deny:") && r.lines[1].contains("default deny"));
        let r = check(&toks, &pols, true, Some(&ex("admin", "Cluster::\"this\""))).unwrap();
        assert!(r.lines[1].contains("builtin:write"), "{:?}", r.lines);
        // An invalid file names the line and @id.
        std::fs::write(
            &pols,
            "\n@id(\"bad\")\npermit(principal, action == Action::\"Read\", resource);",
        )
        .unwrap();
        let r = check(&toks, &pols, false, None).unwrap();
        let e = r.error.unwrap();
        assert!(e.contains("line 3") && e.contains("@id `bad`"), "{e}");
        // A resource-constrained read policy loads (ADR 0013 PR 3 lifted
        // the PR 1 interim rule) and is explained per resource.
        std::fs::write(
            &pols,
            "@id(\"r\")\npermit(principal, action == Action::\"read\", resource in Org::\"a\");",
        )
        .unwrap();
        let r = check(&toks, &pols, false, None).unwrap();
        assert!(r.error.is_none(), "{:?}", r.error);
    }
}
