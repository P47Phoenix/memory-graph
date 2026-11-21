use super::*;
use proptest::prelude::*;

fn set(text: &str) -> PolicySet {
    PolicySet::new(parse(text.as_bytes()).unwrap_or_else(|e| panic!("{e}: {text}")))
}

fn perr(text: &str) -> ParseError {
    parse(text.as_bytes()).expect_err(text)
}

fn groups(g: &[&str]) -> Vec<String> {
    g.iter().map(|s| s.to_string()).collect()
}

fn who<'a>(token: &'a str, g: &'a [String], role: Role) -> Principal<'a> {
    Principal {
        token,
        groups: g,
        role,
    }
}

const ADR_EXAMPLE: &str = r#"// policies.cedar, next to tokens.toml
@id("acme-read")
permit(principal in Group::"acme-readers", action in [Action::"read", Action::"search"], resource in Org::"acme");

@id("web-ci")
permit(principal == Token::"web-ci", action in [Action::"index", Action::"prune"], resource == Repo::"acme/web");

@id("legal-frozen")
forbid(principal, action in [Action::"index", Action::"prune", Action::"delete"], resource in Repo::"acme/legal");
"#;

#[test]
fn the_adr_example_parses_and_decides() {
    let s = set(ADR_EXAMPLE);
    assert_eq!(s.len(), 3);
    let g = groups(&["acme-readers"]);
    let reader = who("bob", &g, Role::Read);
    assert!(
        s.decide(&reader, Action::Read, &Resource::Repo("acme", "web"))
            .allowed
    );
    assert!(
        s.decide(&reader, Action::Search, &Resource::Org("acme"))
            .allowed
    );
    assert!(
        !s.decide(&reader, Action::Read, &Resource::Repo("other", "web"))
            .allowed
    );
    let none: Vec<String> = vec![];
    let ci = who("web-ci", &none, Role::Write);
    let d = s.decide(&ci, Action::Index, &Resource::Repo("acme", "web"));
    assert!(d.allowed);
    assert_eq!(d.matched, vec!["web-ci".to_string()]);
    assert!(
        !s.decide(&ci, Action::Index, &Resource::Repo("acme", "api"))
            .allowed
    );
    assert!(
        !s.decide(&ci, Action::Read, &Resource::Repo("acme", "web"))
            .allowed,
        "index but not read"
    );
}

#[test]
fn forbid_beats_permit_at_every_level() {
    let none: Vec<String> = vec![];
    let g = groups(&["g"]);
    let cases = [
        // org permit + repo forbid
        (
            r#"permit(principal, action, resource in Org::"o"); forbid(principal, action, resource == Repo::"o/r");"#,
            Resource::Repo("o", "r"),
        ),
        // repo permit + org forbid
        (
            r#"permit(principal, action, resource == Repo::"o/r"); forbid(principal, action, resource in Org::"o");"#,
            Resource::Repo("o", "r"),
        ),
        // group permit + token forbid
        (
            r#"permit(principal in Group::"g", action, resource); forbid(principal == Token::"t", action, resource);"#,
            Resource::Cluster,
        ),
        // token permit + group forbid
        (
            r#"permit(principal == Token::"t", action, resource); forbid(principal in Group::"g", action, resource);"#,
            Resource::Cluster,
        ),
        // role permit + any forbid
        (
            r#"permit(principal in Role::"write", action, resource); forbid(principal, action == Action::"admin", resource);"#,
            Resource::Cluster,
        ),
    ];
    for (text, res) in cases {
        let s = set(text);
        let d = s.decide(&who("t", &g, Role::Write), Action::Admin, &res);
        assert!(d.permitted && d.forbidden && !d.allowed, "{text}");
        assert_eq!(d.matched.len(), 2, "{text}");
        // Order never matters.
        let rev: Vec<Policy> = parse(text.as_bytes()).unwrap().into_iter().rev().collect();
        assert!(
            !PolicySet::new(rev)
                .decide(&who("t", &g, Role::Write), Action::Admin, &res)
                .allowed
        );
    }
    let _ = none;
}

#[test]
fn default_deny() {
    let none: Vec<String> = vec![];
    let s = set("");
    for a in Action::ALL {
        for r in [
            Resource::Cluster,
            Resource::Org("o"),
            Resource::Repo("o", "r"),
        ] {
            let d = s.decide(&who("t", &none, Role::Write), a, &r);
            assert!(!d.allowed && !d.permitted && d.matched.is_empty());
        }
    }
    // Only a forbid: still deny.
    let s = set("forbid(principal, action, resource);");
    assert!(
        !s.decide(
            &who("t", &none, Role::Write),
            Action::Read,
            &Resource::Cluster
        )
        .allowed
    );
}

#[test]
fn the_role_is_a_ceiling() {
    let none: Vec<String> = vec![];
    let s = set(r#"permit(principal == Token::"r", action, resource);"#);
    let r = who("r", &none, Role::Read);
    for a in Action::ALL {
        let want = matches!(a, Action::Read | Action::Search | Action::Status);
        assert_eq!(s.decide(&r, a, &Resource::Cluster).allowed, want, "{a:?}");
    }
    let w = who("r", &none, Role::Write);
    assert!(Action::ALL
        .iter()
        .all(|a| s.decide(&w, *a, &Resource::Cluster).allowed));
}

#[test]
fn builtins_equal_adr_0012_roles() {
    let none: Vec<String> = vec![];
    let s = PolicySet::builtins();
    for a in Action::ALL {
        for res in [
            Resource::Cluster,
            Resource::Org("o"),
            Resource::Repo("o", "r"),
        ] {
            let r = s.decide(&who("x", &none, Role::Read), a, &res).allowed;
            assert_eq!(
                r,
                matches!(a, Action::Read | Action::Search | Action::Status),
                "{a:?}"
            );
            assert!(s.decide(&who("x", &none, Role::Write), a, &res).allowed);
        }
    }
    let with = set("").with_builtins();
    assert_eq!(with.len(), 2);
    assert!(
        with.decide(
            &who("x", &none, Role::Write),
            Action::Admin,
            &Resource::Cluster
        )
        .allowed
    );
}

#[test]
fn in_versus_eq_and_the_org_repo_split() {
    let none: Vec<String> = vec![];
    let p = who("t", &none, Role::Write);
    let s = set(r#"permit(principal, action, resource == Org::"o");"#);
    assert!(s.decide(&p, Action::Index, &Resource::Org("o")).allowed);
    assert!(
        !s.decide(&p, Action::Index, &Resource::Repo("o", "r"))
            .allowed,
        "== is not in"
    );
    let s = set(r#"permit(principal, action, resource in Org::"o");"#);
    assert!(s.decide(&p, Action::Index, &Resource::Org("o")).allowed);
    assert!(
        s.decide(&p, Action::Index, &Resource::Repo("o", "r"))
            .allowed
    );
    assert!(
        !s.decide(&p, Action::Index, &Resource::Repo("oo", "r"))
            .allowed
    );
    assert!(!s.decide(&p, Action::Index, &Resource::Cluster).allowed);
    // Split at the first `/`.
    let s = set(r#"permit(principal, action, resource == Repo::"a/b/c");"#);
    assert!(
        s.decide(&p, Action::Index, &Resource::Repo("a", "b/c"))
            .allowed
    );
    assert!(
        !s.decide(&p, Action::Index, &Resource::Repo("a/b", "c"))
            .allowed
    );
    let s = set(r#"permit(principal, action, resource in Repo::"a/b");"#);
    assert!(
        s.decide(&p, Action::Index, &Resource::Repo("a", "b"))
            .allowed
    );
    assert!(!s.decide(&p, Action::Index, &Resource::Org("a")).allowed);
    let s = set(r#"permit(principal, action, resource in Cluster::"this");"#);
    assert!(s.decide(&p, Action::Index, &Resource::Cluster).allowed);
    assert!(!s.decide(&p, Action::Index, &Resource::Org("a")).allowed);
    // principal in Token == principal == Token.
    let s = set(r#"permit(principal in Token::"t", action, resource);"#);
    assert!(s.decide(&p, Action::Index, &Resource::Cluster).allowed);
    assert!(
        !s.decide(
            &who("u", &none, Role::Write),
            Action::Index,
            &Resource::Cluster
        )
        .allowed
    );
    // principal == Group never matches.
    let g = groups(&["g"]);
    let s = set(r#"permit(principal == Group::"g", action, resource);"#);
    assert!(
        !s.decide(
            &who("t", &g, Role::Write),
            Action::Index,
            &Resource::Cluster
        )
        .allowed
    );
}

#[test]
fn the_action_bitset() {
    let none: Vec<String> = vec![];
    let p = who("t", &none, Role::Write);
    let s = set(r#"permit(principal, action in [Action::"prune", Action::"delete"], resource);"#);
    for a in Action::ALL {
        assert_eq!(
            s.decide(&p, a, &Resource::Cluster).allowed,
            matches!(a, Action::Prune | Action::Delete),
            "{a:?}"
        );
    }
    let s = set(r#"permit(principal, action == Action::"status", resource);"#);
    assert!(s.decide(&p, Action::Status, &Resource::Cluster).allowed);
    assert!(!s.decide(&p, Action::Admin, &Resource::Cluster).allowed);
    let s = set(r#"permit(principal, action in Action::"admin", resource);"#);
    assert!(s.decide(&p, Action::Admin, &Resource::Cluster).allowed);
    assert!(!s.decide(&p, Action::Status, &Resource::Cluster).allowed);
    let s = set(r#"permit(principal, action in [], resource);"#);
    assert!(Action::ALL
        .iter()
        .all(|a| !s.decide(&p, *a, &Resource::Cluster).allowed));
}

#[test]
fn org_wide_prune_and_delete_follow_d6() {
    let none: Vec<String> = vec![];
    let p = who("t", &none, Role::Write);
    let ow = |text: &str| set(text).decide_org_wide(&p, Action::Prune, "o").allowed;
    assert!(ow(r#"permit(principal, action, resource in Org::"o");"#));
    assert!(ow(r#"permit(principal, action, resource == Org::"o");"#));
    assert!(ow(r#"permit(principal, action, resource);"#));
    assert!(ow(
        r#"permit(principal, action, resource == Cluster::"this");"#
    ));
    // Per-repo grants never add up to an org-wide one.
    assert!(!ow(
        r#"permit(principal, action, resource == Repo::"o/a"); permit(principal, action, resource == Repo::"o/b");"#
    ));
    // A forbid on one repo inside denies it.
    assert!(!ow(
        r#"permit(principal, action, resource in Org::"o"); forbid(principal, action == Action::"prune", resource == Repo::"o/legal");"#
    ));
    assert!(!ow(
        r#"permit(principal, action, resource); forbid(principal, action, resource in Repo::"o/legal");"#
    ));
    // ... but not one in another org, or for another action.
    assert!(ow(
        r#"permit(principal, action, resource in Org::"o"); forbid(principal, action, resource == Repo::"p/legal");"#
    ));
    assert!(ow(
        r#"permit(principal, action, resource in Org::"o"); forbid(principal, action == Action::"index", resource == Repo::"o/legal");"#
    ));
    // A forbid on the org, or the cluster.
    assert!(!ow(
        r#"permit(principal, action, resource == Cluster::"this"); forbid(principal, action, resource in Org::"o");"#
    ));
    assert!(!ow(
        r#"permit(principal, action, resource in Org::"o"); forbid(principal, action, resource == Cluster::"this");"#
    ));
    // A read token: never.
    assert!(
        !set("permit(principal, action, resource);")
            .decide_org_wide(&who("t", &none, Role::Read), Action::Prune, "o")
            .allowed
    );
}

#[test]
fn resource_constrained_read_policies_load() {
    // ADR 0013 PR 3 lifted the PR 1 interim rule: a read or search policy
    // may constrain its resource.
    for text in [
        "permit(principal, action == Action::\"read\", resource in Org::\"a\");",
        "forbid(principal, action == Action::\"search\", resource == Repo::\"a/b\");",
        "permit(principal, action, resource == Repo::\"a/b\");",
    ] {
        parse(text.as_bytes()).unwrap();
    }
}

#[test]
fn read_rules_follow_the_policy_forms() {
    let none: Vec<String> = Vec::new();
    let p = who("t", &none, Role::Read);
    let r = set(
        r#"permit(principal, action == Action::"read", resource in Org::"a");
forbid(principal, action == Action::"read", resource == Repo::"a/x");
permit(principal, action == Action::"read", resource == Org::"b");
permit(principal, action == Action::"read", resource == Repo::"c/y");
permit(principal, action == Action::"search", resource);
permit(principal, action == Action::"read", resource == Cluster::"this");"#,
    )
    .read_rules(&p, Action::Read);
    assert!(r.repo_visible("a", "w") && !r.repo_visible("a", "x"));
    assert!(r.org_granted("a"), "a repo forbid does not deny the org");
    assert!(
        r.org_granted("b") && !r.repo_visible("b", "w"),
        "== Org: the org alone"
    );
    assert!(r.repo_visible("c", "y") && !r.org_granted("c") && !r.repo_visible("c", "z"));
    assert!(
        !r.any,
        "a search permit is not a read permit; Cluster grants no repo"
    );
    // A role below the action gets nothing.
    let w = set("permit(principal, action, resource);");
    assert_eq!(w.read_rules(&p, Action::Index), crate::ReadRules::default());
}

mod read_rules_oracle {
    use super::*;

    const ORGS: &[&str] = &["a", "b", "c"];
    const REPOS: &[(&str, &str)] = &[("a", "x"), ("a", "y"), ("b", "x"), ("c", "z")];

    fn resource() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("resource".to_string()),
            Just("resource == Cluster::\"this\"".to_string()),
            proptest::sample::select(ORGS).prop_flat_map(|o| prop_oneof![
                Just(format!("resource == Org::\"{o}\"")),
                Just(format!("resource in Org::\"{o}\"")),
            ]),
            proptest::sample::select(REPOS).prop_flat_map(|(o, r)| prop_oneof![
                Just(format!("resource == Repo::\"{o}/{r}\"")),
                Just(format!("resource in Repo::\"{o}/{r}\"")),
            ]),
        ]
    }

    fn policy() -> impl Strategy<Value = String> {
        (
            any::<bool>(),
            prop_oneof![
                Just("principal"),
                Just("principal == Token::\"t\""),
                Just("principal == Token::\"u\""),
                Just("principal in Group::\"g\""),
                Just("principal in Role::\"read\""),
                Just("principal in Role::\"write\""),
            ],
            prop_oneof![
                Just("action"),
                Just("action == Action::\"read\""),
                Just("action == Action::\"search\""),
                Just("action in [Action::\"read\", Action::\"index\"]"),
            ],
            resource(),
        )
            .prop_map(|(permit, p, a, r)| {
                format!(
                    "{}({p}, {a}, {r});",
                    if permit { "permit" } else { "forbid" }
                )
            })
    }

    proptest! {
        /// The compiled scope decides exactly as `decide` for every org and
        /// repo name (ADR 0013 D3, D10 mutation targets).
        #[test]
        fn read_rules_agree_with_decide(
            pols in proptest::collection::vec(policy(), 0..8),
            write in any::<bool>(),
            in_group in any::<bool>(),
        ) {
            let text = pols.join("\n");
            let s = set(&text);
            let groups: Vec<String> = if in_group { vec!["g".into()] } else { vec![] };
            let p = who("t", &groups, if write { Role::Write } else { Role::Read });
            for action in [Action::Read, Action::Search] {
                let r = s.read_rules(&p, action);
                for (o, rp) in REPOS.iter().chain(&[("a", "new"), ("d", "x")]) {
                    prop_assert_eq!(
                        r.repo_visible(o, rp),
                        s.decide(&p, action, &Resource::Repo(o, rp)).allowed,
                        "{} on {}/{}", text, o, rp
                    );
                }
                for o in ORGS.iter().chain(&["d"]) {
                    prop_assert_eq!(
                        r.org_granted(o),
                        s.decide(&p, action, &Resource::Org(o)).allowed,
                        "{} on org {}", text, o
                    );
                }
            }
        }
    }
}

#[test]
fn unknown_principals_warn_and_match_nothing() {
    let s = set(
        r#"@id("a") permit(principal == Token::"gone", action, resource);
@id("b") permit(principal in Group::"nobody", action, resource);"#,
    );
    let w = s.lint(&|t| t == "here", &|g| g == "team");
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(w[0].contains("gone") && w[0].contains("@id `a`"));
    assert!(w[1].contains("nobody"));
    assert!(
        set(r#"permit(principal == Token::"here", action, resource);"#)
            .lint(&|t| t == "here", &|_| false)
            .is_empty()
    );
    let w = set("forbid(principal, action, resource);").lint(&|_| true, &|_| true);
    assert!(w.iter().any(|m| m.contains("only `forbid`")));
    let w = set("permit(principal, action in [], resource);").lint(&|_| true, &|_| true);
    assert!(w.iter().any(|m| m.contains("action in []")));
}

// --- D2 edge cases, one test each -------------------------------------------

#[test]
fn empty_and_comments_only_files_are_empty_sets() {
    assert!(parse(b"").unwrap().is_empty());
    assert!(parse(b"// nothing\n// still nothing\n").unwrap().is_empty());
    assert!(parse(b"   \r\n\t").unwrap().is_empty());
}

#[test]
fn a_bom_is_skipped() {
    let mut b = vec![0xEF, 0xBB, 0xBF];
    b.extend_from_slice(b"permit(principal, action, resource);");
    assert_eq!(parse(&b).unwrap().len(), 1);
    // ... only at the start.
    assert!(parse("permit(principal, action, resource);\u{feff}".as_bytes()).is_err());
}

#[test]
fn crlf_equals_lf() {
    let lf = "@id(\"a\")\npermit(principal,\naction,\nresource);\n";
    let crlf = lf.replace('\n', "\r\n");
    assert_eq!(
        parse(lf.as_bytes()).unwrap(),
        parse(crlf.as_bytes()).unwrap()
    );
}

#[test]
fn nul_and_invalid_utf8_are_errors() {
    assert!(perr("permit(principal, action, resource);\0")
        .msg
        .contains("NUL"));
    assert!(perr("\n\"\0\"").line == 2);
    let e = parse(b"permit(principal, action, resource);\n\xff").unwrap_err();
    assert!(e.msg.contains("UTF-8") && e.line == 2, "{e}");
}

#[test]
fn block_comments_are_refused_as_not_valid_cedar() {
    // ADR 0013 D2 (corrected 2026-11-21): Cedar has only `//` comments.
    for t in [
        "/* a */ permit(principal, action, resource);",
        "permit(/* b */principal, action, resource);",
        "\n/* never closed\npermit(principal, action, resource);",
        "/*",
        "/**/",
    ] {
        let e = perr(t);
        assert!(e.msg.contains("not valid Cedar"), "{t}: {e}");
    }
    assert_eq!(perr("\n\n/* x */").line, 3);
    assert_eq!(
        parse(b"// a\npermit(principal, action, resource); // c")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn nfc_and_nfd_names_are_different_resources() {
    let nfc = "permit(principal, action, resource == Org::\"caf\u{e9}\");";
    let s = set(nfc);
    let none: Vec<String> = vec![];
    let p = who("t", &none, Role::Write);
    assert!(
        s.decide(&p, Action::Index, &Resource::Org("caf\u{e9}"))
            .allowed
    );
    assert!(
        !s.decide(&p, Action::Index, &Resource::Org("cafe\u{301}"))
            .allowed
    );
    assert!(s.lint(&|_| true, &|_| true).is_empty());
    let nfd = set("permit(principal, action, resource == Org::\"cafe\u{301}\");");
    assert!(nfd
        .lint(&|_| true, &|_| true)
        .iter()
        .any(|w| w.contains("NFC")));
}

#[test]
fn every_escape_and_invalid_escapes() {
    let s = set(
        r#"permit(principal == Token::"a\"b\\c\nd\re\tf\0g\'h\x41\u{1F600}\u{e9}", action, resource);"#,
    );
    match &s.policies()[0].principal {
        Scope::Eq(e) => assert_eq!(e.id, "a\"b\\c\nd\re\tf\0g'hA\u{1F600}\u{e9}"),
        other => panic!("{other:?}"),
    }
    for bad in [
        r#"\q"#,
        r#"\x8f"#,
        r#"\x4"#,
        r#"\u{}"#,
        r#"\u{1234567}"#,
        r#"\u{d800}"#,
        "\\u1234",
        r#"\u{110000}"#,
        r#"\"#,
    ] {
        let t = format!("permit(principal == Token::\"{bad}\", action, resource);");
        assert!(parse(t.as_bytes()).is_err(), "{bad}");
    }
}

#[test]
fn namespaced_and_unknown_types() {
    assert!(
        perr(r#"permit(principal, action, resource == Foo::Repo::"a/b");"#)
            .msg
            .contains("namespaced")
    );
    assert!(
        perr(r#"permit(principal, action == Foo::Action::"read", resource);"#)
            .msg
            .contains("namespaced")
    );
    assert!(perr(r#"permit(principal, action, resource == File::"x");"#)
        .msg
        .contains("unknown entity type"));
    assert!(
        perr(r#"permit(principal == Repo::"a/b", action, resource);"#)
            .msg
            .contains("cannot be a principal")
    );
    assert!(
        perr(r#"permit(principal, action, resource == Token::"a");"#)
            .msg
            .contains("cannot be a resource")
    );
    assert!(
        perr(r#"permit(principal in Role::"admin", action, resource);"#)
            .msg
            .contains("role")
    );
    assert!(
        perr(r#"permit(principal, action, resource == Cluster::"that");"#)
            .msg
            .contains("Cluster")
    );
}

#[test]
fn action_names_are_case_sensitive() {
    assert!(
        perr(r#"permit(principal, action == Action::"Read", resource);"#)
            .msg
            .contains("unknown action")
    );
    assert!(
        perr(r#"permit(principal, action in [Action::"read", Action::"fly"], resource);"#)
            .msg
            .contains("unknown action")
    );
    assert!(
        perr(r#"permit(principal, action == Thing::"read", resource);"#)
            .msg
            .contains("Action::")
    );
}

#[test]
fn repo_ids_need_org_and_repo() {
    for bad in ["ab", "/b", "a/", "/"] {
        let t = format!("permit(principal, action, resource == Repo::\"{bad}\");");
        assert!(
            parse(t.as_bytes()).unwrap_err().msg.contains("org/repo"),
            "{bad}"
        );
    }
}

#[test]
fn outside_the_grammar_is_an_error() {
    for bad in [
        "permit(principal, action, resource) when { true };",
        "permit(principal, action, resource) unless { false };",
        "permit(principal is Token, action, resource);",
        "permit(principal, action, resource is Repo);",
        "permit(?principal, action, resource);",
        "permit(principal == ?principal, action, resource);",
        "allow(principal, action, resource);",
        "permit(principal, action, resource)",
        "permit(principal, action, resource,);",
        "permit(principal, action in [Action::\"read\",], resource);",
        "permit(principal, resource, action);",
        "permit(principal, action, resource); extra",
        "permit(principal != Token::\"a\", action, resource);",
        "permit(principal, action, resource like \"x\");",
        "@id permit(principal, action, resource);",
        "@id(\"a\" permit(principal, action, resource);",
        "permit(principal == Token:\"a\", action, resource);",
    ] {
        assert!(parse(bad.as_bytes()).is_err(), "accepted: {bad}");
    }
}

#[test]
fn annotations() {
    let s = set("@id(\"a\") @owner(\"me\") @flag permit(principal, action, resource);\npermit(principal, action, resource);");
    assert_eq!(s.policies()[0].id, "a");
    assert_eq!(s.policies()[1].id, "policy1");
    assert_eq!(s.policies()[0].annotations.len(), 3);
    let e = perr("@id(\"a\") @id(\"b\") permit(principal, action, resource);");
    assert!(e.msg.contains("duplicate annotation"));
    let e = perr("@owner(\"a\") @owner(\"b\") permit(principal, action, resource);");
    assert!(e.msg.contains("duplicate annotation"));
    let e = perr("@id(\"a\") permit(principal, action, resource);\n@id(\"a\") permit(principal, action, resource);");
    assert!(
        e.msg.contains("duplicate `@id`") && e.line == 2 && e.id.as_deref() == Some("a"),
        "{e}"
    );
    // Errors name the @id of the policy they are in.
    let e = perr("@id(\"x\")\npermit(principal, action == Action::\"nope\", resource);");
    assert_eq!(e.id.as_deref(), Some("x"));
    assert_eq!(e.line, 2);
}

#[test]
fn limits_at_n_and_n_plus_one() {
    let one = "permit(principal,action,resource);\n";
    let ok = one.repeat(MAX_POLICIES);
    assert_eq!(parse(ok.as_bytes()).unwrap().len(), MAX_POLICIES);
    let e = parse(one.repeat(MAX_POLICIES + 1).as_bytes()).unwrap_err();
    assert!(e.msg.contains("more than"));
    let mut exact = one.repeat(10).into_bytes();
    exact.resize(MAX_FILE_BYTES, b' ');
    assert_eq!(parse(&exact).unwrap().len(), 10);
    exact.push(b' ');
    assert!(parse(&exact).unwrap_err().msg.contains("bytes"));
}

// --- The D10 gate: policies visited per decision ----------------------------

#[test]
fn policies_visited_per_decision() {
    // 20 tokens, 5 groups, wildcard and role policies.
    let mut text = String::new();
    for t in 0..20 {
        text.push_str(&format!("permit(principal == Token::\"t{t}\", action == Action::\"index\", resource == Repo::\"o/r{t}\");\n"));
        text.push_str(&format!(
            "forbid(principal in Token::\"t{t}\", action == Action::\"prune\", resource);\n"
        ));
    }
    for g in 0..5 {
        for k in 0..3 {
            text.push_str(&format!("permit(principal in Group::\"g{g}\", action == Action::\"read\", resource);// {k}\n"));
        }
    }
    text.push_str("permit(principal in Role::\"read\", action == Action::\"status\", resource);\n");
    text.push_str(
        "permit(principal in Role::\"write\", action == Action::\"status\", resource);\n",
    );
    text.push_str("permit(principal in Role::\"write\", action == Action::\"admin\", resource);\n");
    text.push_str("forbid(principal, action == Action::\"delete\", resource);\n");
    text.push_str("permit(principal == Group::\"g0\", action, resource);\n"); // in no bucket
    let s = set(&text);
    let g = groups(&["g1", "g3", "g3", "missing"]);
    // token bucket 2 + g1 3 + g3 3 (once) + read role 1 + write role 2 + any 1.
    assert_eq!(
        s.decide(
            &who("t7", &g, Role::Write),
            Action::Index,
            &Resource::Repo("o", "r7")
        )
        .visited,
        12
    );
    assert_eq!(
        s.decide(&who("t7", &g, Role::Read), Action::Read, &Resource::Cluster)
            .visited,
        10
    );
    assert_eq!(
        s.decide(
            &who("nobody", &[], Role::Write),
            Action::Read,
            &Resource::Cluster
        )
        .visited,
        4
    );
    // Above the role ceiling: nothing visited.
    assert_eq!(
        s.decide(
            &who("t7", &g, Role::Read),
            Action::Admin,
            &Resource::Cluster
        )
        .visited,
        0
    );
}

#[test]
fn five_hundred_wildcards_visit_exactly_five_hundred() {
    let mut text = String::new();
    for i in 0..500 {
        text.push_str(&format!(
            "permit(principal, action == Action::\"index\", resource == Repo::\"o/r{i}\");\n"
        ));
    }
    for i in 0..300 {
        text.push_str(&format!(
            "permit(principal == Token::\"other{i}\", action, resource);\n"
        ));
    }
    let s = set(&text);
    let d = s.decide(
        &who("t", &[], Role::Write),
        Action::Index,
        &Resource::Repo("o", "r499"),
    );
    assert_eq!(d.visited, 500);
    assert!(d.allowed);
}

/// Recorded, not asserted (D10): decision latency.
#[test]
#[ignore]
fn bench_decide() {
    let mut text = String::new();
    for i in 0..500 {
        text.push_str(&format!(
            "permit(principal, action == Action::\"index\", resource == Repo::\"o/r{i}\");\n"
        ));
    }
    let s = set(&text);
    let mut v = Vec::new();
    for i in 0..20_000u32 {
        let r = format!("r{}", i % 600);
        let t = std::time::Instant::now();
        let d = s.decide(
            &who("t", &[], Role::Write),
            Action::Index,
            &Resource::Repo("o", &r),
        );
        v.push(t.elapsed());
        std::hint::black_box(d);
    }
    v.sort();
    println!(
        "500 wildcards: p50 {:?} p99 {:?}",
        v[v.len() / 2],
        v[v.len() * 99 / 100]
    );
}

// --- Round trip and fuzz -----------------------------------------------------

fn name() -> BoxedStrategy<String> {
    prop_oneof![
        "[a-z]{1,4}",
        "[a-z]{1,3}/[a-z/]{1,4}",
        any::<String>().prop_map(|s| s.chars().take(6).collect()),
    ]
    .boxed()
}

fn entity(principal: bool) -> impl Strategy<Value = Entity> {
    let n = name();
    if principal {
        prop_oneof![
            n.clone().prop_map(|id| Entity { ty: Ty::Token, id }),
            n.prop_map(|id| Entity { ty: Ty::Group, id }),
            prop_oneof![Just("read"), Just("write")].prop_map(|r| Entity {
                ty: Ty::Role,
                id: r.into()
            }),
        ]
        .boxed()
    } else {
        prop_oneof![
            n.clone().prop_map(|id| Entity { ty: Ty::Org, id }),
            (
                "[a-z]{1,3}",
                n.prop_filter("a repo part is non-empty", |s| !s.is_empty())
            )
                .prop_map(|(o, r)| Entity {
                    ty: Ty::Repo,
                    id: format!("{o}/{r}")
                }),
            Just(Entity {
                ty: Ty::Cluster,
                id: "this".into()
            }),
        ]
        .boxed()
    }
}

fn scope(principal: bool) -> impl Strategy<Value = Scope> {
    prop_oneof![
        Just(Scope::Any),
        entity(principal).prop_map(Scope::Eq),
        entity(principal).prop_map(Scope::In),
    ]
}

fn action() -> impl Strategy<Value = Action> {
    (0usize..8).prop_map(|i| Action::ALL[i])
}

prop_compose! {
    fn policy()(
        id in proptest::option::of("[a-z0-9:-]{1,8}"),
        other in proptest::option::of(proptest::option::of(any::<String>())),
        forbid in any::<bool>(),
        principal in scope(true),
        act in prop_oneof![
            Just(ActionScope::Any),
            action().prop_map(ActionScope::Eq),
            proptest::collection::vec(action(), 0..4).prop_map(ActionScope::In),
        ],
        resource in scope(false),
    ) -> Policy {
        let mut annotations = Vec::new();
        if let Some(i) = &id { annotations.push(("id".to_string(), Some(i.clone()))); }
        if let Some(o) = other { annotations.push(("note".to_string(), o)); }
        Policy {
            id: id.unwrap_or_default(),
            annotations,
            effect: if forbid { Effect::Forbid } else { Effect::Permit },
            principal,
            action: act,
            resource,
            line: 0,
        }
    }
}

fn strip(mut v: Vec<Policy>) -> Vec<Policy> {
    for p in &mut v {
        p.line = 0;
    }
    v
}

proptest! {
    #[test]
    fn the_parser_never_panics_on_bytes(b in proptest::collection::vec(any::<u8>(), 0..300)) {
        let _ = parse(&b);
        let _ = parse_resource(&String::from_utf8_lossy(&b));
    }

    #[test]
    fn the_parser_never_panics_on_near_grammar_text(s in r#"(@id\("[a-z]{0,3}"\) )?(permit|forbid|when|is)\((principal|resource|action|\?principal)( (==|in|is) )?([A-Za-z]{1,6}(::[A-Za-z]{1,6})?::"[a-z/\\]{0,4}")?(, ?|\)|;| |\[|\]){0,6}"#) {
        let _ = parse(s.as_bytes());
    }

    #[test]
    fn print_parse_round_trips(ps in proptest::collection::vec(policy(), 0..6)) {
        // Unique @ids, as the parser requires.
        let mut seen = std::collections::BTreeSet::new();
        let ps: Vec<Policy> = ps.into_iter().filter(|p| {
            !p.annotations.iter().any(|(k, _)| k == "id") || seen.insert(p.id.clone())
        }).collect();
        let text: String = ps.iter().map(|p| format!("{p}\n")).collect();
        let back = strip(parse(text.as_bytes()).map_err(|e| TestCaseError::fail(format!("{e}\n{text}")))?);
        prop_assert_eq!(back.len(), ps.len());
        for (i, (a, mut b)) in ps.into_iter().zip(back).enumerate() {
            if !a.annotations.iter().any(|(k, _)| k == "id") {
                prop_assert_eq!(&b.id, &format!("policy{i}"));
                b.id = a.id.clone();
            }
            prop_assert_eq!(a, b);
        }
    }

    /// Anything the parser accepts prints back to text it accepts with the
    /// same meaning (and the same decisions).
    #[test]
    fn accepted_text_reprints_identically(ps in proptest::collection::vec(policy(), 1..4), tok in "[a-z]{1,2}", a in action(), org in "[a-z]{1,2}", repo in "[a-z]{1,2}") {
        let text: String = ps.iter().map(|p| format!("{p}\n")).collect();
        if let Ok(first) = parse(text.as_bytes()) {
            let again: String = first.iter().map(|p| format!("{p}\n")).collect();
            let second = parse(again.as_bytes()).unwrap();
            prop_assert_eq!(strip(first.clone()), strip(second.clone()));
            let g = vec![tok.clone()];
            let p = who(&tok, &g, Role::Write);
            for res in [Resource::Org(&org), Resource::Repo(&org, &repo), Resource::Cluster] {
                prop_assert_eq!(PolicySet::new(first.clone()).decide(&p, a, &res).allowed, PolicySet::new(second.clone()).decide(&p, a, &res).allowed);
            }
        }
    }
}

// --- Replay fixtures from the Cedar oracle (story 72a) ----------------------

/// Every `testdata/authz/*.cedar` with its `.requests`: one request per
/// line, tab-separated `token  groups(,)  role  action  resource  allow|deny`.
#[test]
fn oracle_replay_fixtures() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/authz");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).expect("testdata/authz") {
        let path = e.unwrap().path();
        if path.extension().and_then(|x| x.to_str()) != Some("cedar") {
            continue;
        }
        let s = PolicySet::new(parse(&std::fs::read(&path).unwrap()).unwrap());
        let reqs = std::fs::read_to_string(path.with_extension("requests")).unwrap();
        for line in reqs
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        {
            let f: Vec<&str> = line.trim_end().split('\t').collect();
            assert_eq!(f.len(), 6, "{}: {line}", path.display());
            let groups: Vec<String> = f[1]
                .split(',')
                .filter(|g| !g.is_empty())
                .map(String::from)
                .collect();
            let role = if f[2] == "read" {
                Role::Read
            } else {
                Role::Write
            };
            let action = Action::parse(f[3]).unwrap();
            let ent = parse_resource(f[4]).unwrap();
            let got = s
                .decide(
                    &who(f[0], &groups, role),
                    action,
                    &ent.as_resource().unwrap(),
                )
                .allowed;
            assert_eq!(got, f[5] == "allow", "{}: {line}", path.display());
            n += 1;
        }
    }
    assert!(n > 0, "no replay fixtures found");
}

#[test]
fn normalized_ignores_order_and_formatting() {
    let a = set("@id(\"a\") permit(principal, action, resource);\n@id(\"b\")\nforbid(principal,action,resource);");
    let b = set("// x\n@id(\"b\") forbid( principal , action , resource ) ;\n@id(\"a\")permit(principal,action,resource);");
    assert_eq!(a.normalized(), b.normalized());
    assert_ne!(
        a.normalized(),
        set("@id(\"a\") permit(principal, action, resource);").normalized()
    );
}

#[test]
fn parse_resource_for_explain() {
    assert_eq!(
        parse_resource("Repo::\"a/b/c\"").unwrap().as_resource(),
        Some(Resource::Repo("a", "b/c"))
    );
    assert_eq!(
        parse_resource("Cluster::\"this\"").unwrap().as_resource(),
        Some(Resource::Cluster)
    );
    assert!(parse_resource("Token::\"a\"").is_err());
    assert!(parse_resource("Org::\"a\" x").is_err());
    assert_eq!(
        Resource::Repo("a", "b\"c").to_string(),
        "Repo::\"a/b\\\"c\""
    );
}

// --- Mutation-testing kills (cargo mutants over lib.rs) ---------------------

#[test]
fn a_repeated_action_still_matches() {
    let s = set(r#"permit(principal, action in [Action::"index", Action::"index"], resource);"#);
    assert!(
        s.decide(
            &who("t", &[], Role::Write),
            Action::Index,
            &Resource::Cluster
        )
        .allowed
    );
    assert_eq!(
        ActionScope::In(vec![Action::Read, Action::Read]).set(),
        Action::Read.bit()
    );
}

#[test]
fn small_accessors() {
    assert_eq!(Role::Read.name(), "read");
    assert_eq!(Role::Write.name(), "write");
    assert!(set("").is_empty());
    assert!(!set("permit(principal, action, resource);").is_empty());
    assert_eq!(READ_ACTIONS, Action::Read.bit() | Action::Search.bit());
}

#[test]
fn org_wide_visits_and_matches_are_counted_once() {
    let s = set(r#"@id("any") permit(principal, action, resource);
@id("f") forbid(principal, action == Action::"prune", resource == Repo::"p/x");"#);
    let d = s.decide_org_wide(&who("t", &[], Role::Write), Action::Prune, "o");
    assert!(d.allowed);
    // decide(Org) 2 + the repo-forbid scan 2 + decide(Cluster) 2.
    assert_eq!(d.visited, 6);
    assert_eq!(d.matched, vec!["any".to_string()], "no duplicates");
    // Above the ceiling: nothing visited at all.
    let d = s.decide_org_wide(&who("t", &[], Role::Read), Action::Prune, "o");
    assert_eq!(d.visited, 0);
    assert!(!d.allowed);
}

#[test]
fn lint_cases() {
    let s = set(r#"permit(principal in Group::"team", action, resource);"#);
    assert!(s.lint(&|_| true, &|g| g == "team").is_empty());
    let s = set(r#"permit(principal == Group::"team", action, resource);"#);
    let w = s.lint(&|_| true, &|_| true);
    assert!(w.iter().any(|m| m.contains("never matches")), "{w:?}");
    let s = set(r#"permit(principal in Role::"read", action, resource);"#);
    assert!(s.lint(&|_| true, &|_| true).is_empty());
    let s = set(r#"permit(principal == Role::"read", action, resource);"#);
    assert!(s
        .lint(&|_| true, &|_| true)
        .iter()
        .any(|m| m.contains("never matches")));
}

#[test]
fn control_characters_print_as_unicode_escapes() {
    assert_eq!(escape("a\u{1}b"), "a\\u{1}b");
    let s = set("permit(principal == Token::\"\\u{1}\", action, resource);");
    let printed = s.policies()[0].to_string();
    assert!(printed.contains("\\u{1}"), "{printed}");
    assert_eq!(
        strip(parse(printed.as_bytes()).unwrap()),
        strip(s.policies().to_vec())
    );
}

#[test]
fn lexer_error_messages() {
    assert!(perr("/ permit(principal, action, resource);")
        .msg
        .contains("unexpected character"));
    assert!(perr("permit(principal = Token::\"a\", action, resource);")
        .msg
        .contains("unexpected character"));
    assert!(perr("permit(principal == Token:\"a\", action, resource);")
        .msg
        .contains("unexpected character"));
    assert!(perr("permit(?principal, action, resource);")
        .msg
        .contains("template"));
    for t in ["/**", "/* *", "/*", "/*/"] {
        assert!(perr(t).msg.contains("block comments"), "{t}");
    }
    // A newline inside a string still advances the line.
    let e = perr("permit(principal == Token::\"a\nb\", action == Action::\"x\", resource);");
    assert_eq!(e.line, 2, "{e}");
    for kw in ["when", "unless"] {
        let e = perr(&format!(
            "permit(principal, action, resource) {kw} {{ true }};"
        ));
        assert!(e.msg.contains("conditions"), "{kw}: {e}");
    }
}

#[test]
fn escape_edge_cases() {
    for bad in ["\\x+4", "\\u{0000041}", "\\u{+41}"] {
        let t = format!("permit(principal == Token::\"{bad}\", action, resource);");
        assert!(parse(t.as_bytes()).is_err(), "{bad}");
    }
    // An escape near the start of the input (no index underflow).
    assert_eq!(parse_resource("Org::\"\\u{41}\"").unwrap().id, "A");
    assert_eq!(parse_resource("Org::\"\\x41\"").unwrap().id, "A");
}
