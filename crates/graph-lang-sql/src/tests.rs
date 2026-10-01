use super::*;
use proptest::prelude::*;

type Sym = (String, SymbolKind, String, String);

fn syms(src: &str) -> Vec<Sym> {
    let ex = SqlExtractor.extract(src);
    assert!(!ex.has_errors);
    assert_nested(&ex);
    ex.symbols
        .into_iter()
        .map(|s| {
            let text = src[s.span.start as usize..s.span.end as usize].to_string();
            (s.name, s.kind, s.lang_kind.unwrap(), text)
        })
        .collect()
}

fn find<'a>(s: &'a [Sym], name: &str) -> &'a Sym {
    s.iter()
        .find(|x| x.0 == name)
        .unwrap_or_else(|| panic!("{name} not found in {s:#?}"))
}

/// Every pair of spans nests or is disjoint (what the store requires).
fn assert_nested(ex: &Extraction) {
    for a in &ex.symbols {
        for b in &ex.symbols {
            let (a, b) = (&a.span, &b.span);
            let disjoint = a.end <= b.start || b.end <= a.start;
            let nested =
                (a.start <= b.start && b.end <= a.end) || (b.start <= a.start && a.end <= b.end);
            assert!(disjoint || nested, "partial overlap {a:?} {b:?}");
        }
    }
}

#[test]
fn ansi_and_postgres() {
    let src = "-- schema\ncreate schema app;\nCREATE TABLE IF NOT EXISTS app.users (\n  id int primary key, -- ; not an end\n  name text default ';'\n);\n\
CREATE OR REPLACE VIEW app.v AS SELECT CASE WHEN id > 0 THEN 1 END FROM app.users;\n\
CREATE UNIQUE INDEX users_name ON app.users (name);\nCREATE INDEX ON app.users (id);\n\
create sequence seq_1 start 1;\nCREATE TYPE mood AS ENUM ('sad', 'ok');\n\
CREATE FUNCTION app.touch() RETURNS trigger AS $body$\nBEGIN\n  NEW.x := now(); RETURN NEW;\nEND;\n$body$ LANGUAGE plpgsql;\n\
CREATE TRIGGER t_touch BEFORE UPDATE ON app.users FOR EACH ROW EXECUTE FUNCTION app.touch();\n\
CREATE FUNCTION add(a int, b int) RETURNS int AS $$ SELECT a + b; $$ LANGUAGE sql;\n";
    let s = syms(src);
    assert_eq!(s.len(), 9, "{s:#?}");
    let k = |n: &str| {
        let x = find(&s, n);
        (x.1, x.2.as_str())
    };
    assert_eq!(k("app"), (SymbolKind::Module, "schema"));
    assert_eq!(k("app.users"), (SymbolKind::Type, "table"));
    assert_eq!(k("app.v"), (SymbolKind::Type, "view"));
    assert_eq!(k("users_name"), (SymbolKind::Other, "index"));
    assert_eq!(k("seq_1"), (SymbolKind::Other, "sequence"));
    assert_eq!(k("mood"), (SymbolKind::Type, "type"));
    assert_eq!(k("app.touch"), (SymbolKind::Function, "function"));
    assert_eq!(k("t_touch"), (SymbolKind::Other, "trigger"));
    assert_eq!(k("add"), (SymbolKind::Function, "function"));
    assert!(find(&s, "app.users").3.ends_with("default ';'\n);"));
    assert!(find(&s, "app.touch")
        .3
        .starts_with("CREATE FUNCTION app.touch()"));
    assert!(find(&s, "app.touch")
        .3
        .ends_with("$body$ LANGUAGE plpgsql;"));
    assert_eq!(
        find(&s, "add").3,
        "CREATE FUNCTION add(a int, b int) RETURNS int AS $$ SELECT a + b; $$ LANGUAGE sql;"
    );
}

#[test]
fn tsql_batches_and_brackets() {
    let src = "CREATE PROC [dbo].[GetUser] @id int AS\nSET NOCOUNT ON;\nSELECT * FROM [dbo].[Users] WHERE Id = @id;\nGO\n\
create or alter procedure dbo.Save @x int\nas\nbegin\n  begin try\n    begin tran\n    update t set a = case when @x > 1 then 1 else 2 end;\n    commit\n  end try\n  begin catch\n    rollback\n  end catch\nend\ngo\n\
CREATE TABLE [dbo].[Users] ([Id] INT NOT NULL, [Name] NVARCHAR(50))\nCREATE TRIGGER trg ON dbo.Users AFTER INSERT AS BEGIN SELECT 1 END\nGO 2\n";
    let s = syms(src);
    assert_eq!(s.len(), 4, "{s:#?}");
    let get = find(&s, "dbo.GetUser");
    assert_eq!((get.1, get.2.as_str()), (SymbolKind::Function, "procedure"));
    assert!(get.3.ends_with("WHERE Id = @id;"), "{}", get.3);
    let save = find(&s, "dbo.Save");
    assert!(save.3.ends_with("end catch\nend"), "{}", save.3);
    assert_eq!(
        find(&s, "dbo.Users").3,
        "CREATE TABLE [dbo].[Users] ([Id] INT NOT NULL, [Name] NVARCHAR(50))"
    );
    assert_eq!(
        find(&s, "trg").3,
        "CREATE TRIGGER trg ON dbo.Users AFTER INSERT AS BEGIN SELECT 1 END"
    );
}

#[test]
fn tsql_routine_without_outer_begin_runs_to_go() {
    let src = "CREATE PROC p AS\nIF 1=1 BEGIN SELECT 1; END;\nSELECT 2;\nGO\nCREATE PROC q AS\nWHILE 1=0 BEGIN SELECT 1 END\nSELECT 3;\n";
    let s = syms(src);
    assert_eq!(
        find(&s, "p").3,
        "CREATE PROC p AS\nIF 1=1 BEGIN SELECT 1; END;\nSELECT 2;"
    );
    assert_eq!(
        find(&s, "q").3,
        "CREATE PROC q AS\nWHILE 1=0 BEGIN SELECT 1 END\nSELECT 3;"
    );
}

#[test]
fn only_modifiers_between_create_and_the_object_keyword() {
    let src = "CREATE EXTENSION IF NOT EXISTS hstore WITH SCHEMA public;\nCREATE PUBLICATION pub FOR TABLE t;\n\
CREATE ALGORITHM=MERGE SQL SECURITY INVOKER VIEW v AS SELECT 1;\nCREATE GLOBAL TEMPORARY TABLE tt (x int);\n";
    let s = syms(src);
    let names: Vec<_> = s.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(names, ["v", "tt"]);
}

#[test]
fn plsql_packages() {
    let src = "CREATE OR REPLACE PACKAGE emp_pkg AS\n  PROCEDURE hire(p_name VARCHAR2);\n  FUNCTION total RETURN NUMBER;\nEND emp_pkg;\n/\n\
CREATE OR REPLACE PACKAGE BODY emp_pkg IS\n  g_count NUMBER := 0;\n  PROCEDURE hire(p_name VARCHAR2) IS\n    v NUMBER;\n  BEGIN\n    IF p_name IS NULL THEN RETURN; END IF;\n    INSERT INTO emp VALUES (p_name);\n  END hire;\n  FUNCTION total RETURN NUMBER IS\n  BEGIN\n    RETURN g_count;\n  END;\nEND emp_pkg;\n/\n\
CREATE FUNCTION standalone RETURN NUMBER IS\n  x NUMBER;\nBEGIN\n  x := 1;\n  RETURN x;\nEND standalone;\n/\n";
    let s = syms(src);
    let pk: Vec<_> = s.iter().filter(|x| x.0 == "emp_pkg").collect();
    assert_eq!(pk.len(), 2, "{s:#?}");
    assert_eq!((pk[0].1, pk[0].2.as_str()), (SymbolKind::Module, "package"));
    assert!(pk[0].3.ends_with("END emp_pkg;"));
    assert_eq!(pk[1].2, "package body");
    assert!(pk[1].3.ends_with("END emp_pkg;"));
    let hires: Vec<_> = s.iter().filter(|x| x.0 == "hire").collect();
    assert_eq!(hires.len(), 2);
    assert_eq!(hires[0].3, "PROCEDURE hire(p_name VARCHAR2);");
    assert!(hires[1].3.ends_with("END hire;"), "{}", hires[1].3);
    assert_eq!(hires[1].1, SymbolKind::Function);
    let totals: Vec<_> = s.iter().filter(|x| x.0 == "total").collect();
    assert_eq!(totals[0].3, "FUNCTION total RETURN NUMBER;");
    assert!(totals[1].3.ends_with("RETURN g_count;\n  END;"));
    let f = find(&s, "standalone");
    assert!(f.3.ends_with("END standalone;"), "{}", f.3);
}

#[test]
fn plsql_type_body_members_are_methods() {
    let src = "CREATE TYPE BODY shape AS\n  MEMBER FUNCTION area RETURN NUMBER IS\n  BEGIN RETURN 1; END;\nEND;\n/\n";
    let s = syms(src);
    assert_eq!(find(&s, "shape").1, SymbolKind::Type);
    assert_eq!(find(&s, "shape").2, "type body");
    assert_eq!(find(&s, "area").1, SymbolKind::Method);
}

#[test]
fn mysql_delimiter_and_backticks() {
    let src = "CREATE TABLE `my db`.`orders` (`id` INT) ENGINE=InnoDB;\nDELIMITER //\n\
CREATE DEFINER=`root`@`localhost` PROCEDURE `count_orders`(OUT n INT)\nBEGIN\n  SELECT COUNT(*) INTO n FROM orders;\n  WHILE n > 0 DO SET n = n - 1; END WHILE;\nEND //\n\
CREATE FUNCTION f1() RETURNS INT DETERMINISTIC RETURN 1 //\nDELIMITER ;\nCREATE VIEW v1 AS SELECT 1;\n";
    let s = syms(src);
    assert_eq!(s.len(), 4, "{s:#?}");
    assert_eq!(find(&s, "my db.orders").2, "table");
    let p = find(&s, "count_orders");
    assert!(p.3.starts_with("CREATE DEFINER"));
    assert!(p.3.ends_with("END WHILE;\nEND"), "{}", p.3);
    assert_eq!(
        find(&s, "f1").3,
        "CREATE FUNCTION f1() RETURNS INT DETERMINISTIC RETURN 1"
    );
    assert_eq!(find(&s, "v1").3, "CREATE VIEW v1 AS SELECT 1;");
}

#[test]
fn bom_and_non_ascii_positions() {
    let src = "\u{feff}-- é\nCREATE TABLE ü (x int);\n";
    let ex = SqlExtractor.extract(src);
    let t = &ex.symbols[0];
    assert_eq!(t.name, "ü");
    assert_eq!(
        &src[t.span.start as usize..t.span.end as usize],
        "CREATE TABLE ü (x int);"
    );
    assert_eq!((t.span.start_line, t.span.start_col), (2, 1));
    assert_eq!(t.span.end_col, 24);
}

#[test]
fn malformed_input_degrades() {
    for src in [
        "CREATE",
        "create table",
        "CREATE TABLE t (",
        "CREATE PROCEDURE p AS BEGIN",
        "CREATE FUNCTION f() AS $$ never closed",
        "CREATE PACKAGE p AS PROCEDURE",
        "CREATE PACKAGE BODY p IS END",
        "END; END CASE; create or",
        "DELIMITER\nCREATE TABLE t(x)",
        "DELIMITER $$\nCREATE PROCEDURE p() BEGIN END $$",
        ")))CREATE VIEW v AS SELECT (((",
    ] {
        let ex = SqlExtractor.extract(src);
        assert_nested(&ex);
        for s in &ex.symbols {
            assert!(s.span.end as usize <= src.len());
        }
    }
    let s = syms("DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END $$\nDELIMITER ;\n");
    assert_eq!(find(&s, "p").3, "CREATE PROCEDURE p() BEGIN SELECT 1; END");
}

proptest! {
    #[test]
    fn token_soup_keeps_spans_valid(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("CREATE"), Just("table"), Just("PROCEDURE"), Just("function"), Just("package"),
                Just("body"), Just("type"), Just("or"), Just("replace"), Just("t"), Just("x.y"),
                Just("("), Just(")"), Just(";"), Just("BEGIN"), Just("END"), Just("case"),
                Just("is"), Just("as"), Just("$$"), Just("$a$"), Just("\nGO\n"), Just("\n/\n"),
                Just("\nDELIMITER //\n"), Just("//"), Just("[q]"), Just("`b`"), Just("'s'"),
                Just("--c\n"), Just("if"), Just("index"), Just("on"), Just("é"),
            ],
            0..40,
        )
    ) {
        let src = parts.join(" ");
        let ex = SqlExtractor.extract(&src);
        prop_assert!(!ex.has_errors);
        for s in &ex.symbols {
            prop_assert!(s.span.start < s.span.end && s.span.end as usize <= src.len());
            prop_assert!(ex.tokens.iter().any(|t| t.span.start == s.span.start));
            prop_assert!(ex.tokens.iter().any(|t| t.span.end == s.span.end));
            prop_assert!(!s.name.is_empty());
        }
        assert_nested(&ex);
    }
}

/// #143: reserved words are classed `keyword` in any case; qualified parts,
/// variables, quoted names and common column words stay as they were.
#[test]
fn keywords_are_classed_keyword() {
    let src = "select t.select, [from], name from T where x is not null and @from = 1;\nCREATE TABLE Foo (id int);\n";
    let toks = SqlExtractor.extract(src).tokens;
    let class = |text: &str| {
        toks.iter()
            .filter(|t| t.text == text)
            .map(|t| t.class)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        class("select"),
        [TokenClass::Keyword, TokenClass::Identifier]
    );
    assert_eq!(class("from"), [TokenClass::Keyword, TokenClass::Identifier]);
    for kw in ["where", "is", "not", "null", "and", "CREATE", "TABLE"] {
        assert_eq!(class(kw), [TokenClass::Keyword], "{kw}");
    }
    assert_eq!(class("[from]"), [TokenClass::Identifier], "quoted name");
    assert_eq!(class("name"), [TokenClass::Identifier]);
    assert_eq!(class("Foo"), [TokenClass::Identifier]);
    assert!(SqlExtractor.version().starts_with("sql-scan-2+kw1+tok"));
}
