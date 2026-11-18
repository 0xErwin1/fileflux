//! Open with: field codes inside quoted Exec arguments are refused, and a
//! file name never becomes shell text (T5 S3 recheck probes).

use std::path::Path;

use fileflux::core::apps::{ExecError, expand_exec, takes_file};

const NAME: &str = "/d/$(touch pwned);.txt";

fn argv(exec: &str) -> Result<Vec<String>, ExecError> {
    Ok(expand_exec(exec, Path::new(NAME))?
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect())
}

#[test]
fn quoted_field_codes_are_refused_and_not_listed() {
    for exec in [
        r#"sh -c "echo \"%f\"""#,
        r#"app "--file=%f""#,
        r#"app --file="%f""#,
        r#"app "%U""#,
        r#"app "100%% %f""#,
        r#"app "%""#,
        r#"app "x" "%u" %f"#,
    ] {
        assert!(!takes_file(exec), "{exec}");
        assert_eq!(argv(exec), Err(ExecError::QuotedFieldCode), "{exec}");
    }
}

#[test]
fn single_quotes_are_plain_characters_and_the_name_stays_one_argument() {
    assert!(takes_file("sh -c 'echo %f'"));
    assert_eq!(
        argv("sh -c 'echo %f'"),
        Ok(vec![
            "sh".into(),
            "-c".into(),
            "'echo".into(),
            format!("{NAME}'")
        ])
    );
}

#[test]
fn unquoted_codes_beside_quoted_text_expand_to_one_argument() {
    assert!(takes_file(r#"sh -c "echo hi" %f"#));
    assert_eq!(
        argv(r#"sh -c "echo hi" %f"#),
        Ok(vec![
            "sh".into(),
            "-c".into(),
            "echo hi".into(),
            NAME.into()
        ])
    );
    assert!(takes_file(r#"app "100%%" %f"#));
    assert_eq!(
        argv(r#"app "100%%" %f"#),
        Ok(vec!["app".into(), "100%".into(), NAME.into()])
    );
}

#[test]
fn valid_entries_keep_their_argv() {
    assert_eq!(argv("app %f"), Ok(vec!["app".into(), NAME.into()]));
    assert_eq!(
        argv("app --uri %U"),
        Ok(vec![
            "app".into(),
            "--uri".into(),
            "file:///d/%24%28touch%20pwned%29%3B.txt".into()
        ])
    );
    assert_eq!(
        argv("app pre%fpost"),
        Ok(vec!["app".into(), format!("pre{NAME}post")])
    );
}
