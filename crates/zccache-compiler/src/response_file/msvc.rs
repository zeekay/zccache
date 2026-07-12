//! MSVC-compatible response-file serialization.

pub(super) fn format_rsp_content(args: &[String]) -> Option<String> {
    let mut content = String::new();
    for arg in args {
        if arg.contains(['\r', '\n']) {
            return None;
        }
        let needs_quotes = arg.is_empty()
            || arg.starts_with('@')
            || arg.contains(char::is_whitespace)
            || arg.contains('"');
        if !needs_quotes {
            content.push_str(arg);
            content.push(' ');
            continue;
        }

        // MSVC response files use the Windows argv quote/backslash rules:
        // backslashes before a quote are doubled, and trailing backslashes
        // are doubled before the closing quote.
        content.push('"');
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            if ch == '\\' {
                backslashes += 1;
            } else if ch == '"' {
                content.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                content.push('"');
                backslashes = 0;
            } else {
                content.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                content.push(ch);
            }
        }
        content.extend(std::iter::repeat_n('\\', backslashes * 2));
        content.push_str("\" ");
    }
    if content.ends_with(' ') {
        content.pop();
    }
    content.push('\n');
    Some(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_windows_double_quote_rules() {
        let args = vec![
            r"/FoC:\work tree\out\".to_string(),
            r#"/DVALUE="hello world""#.to_string(),
            "source file.cpp".to_string(),
        ];
        let content = format_rsp_content(&args).unwrap();
        assert!(!content.contains('\''));
        assert_eq!(
            content,
            "\"/FoC:\\work tree\\out\\\\\" \"/DVALUE=\\\"hello world\\\"\" \"source file.cpp\"\n"
        );
    }

    #[test]
    fn rejects_newlines() {
        assert!(format_rsp_content(&["/Done\nvalue".to_string()]).is_none());
    }
}
