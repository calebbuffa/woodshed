use super::invalid;
use crate::{FileMatch, Result};

pub(super) fn select(text: &str, rule: &FileMatch) -> Result<String> {
    let variable = rule.cmake_variable();
    let name = variable.or(rule.cmake_url()).expect("validated selector");
    if name.is_empty()
        || name
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || "_-.".contains(c)))
    {
        return Err(invalid(
            "CMake selector must name a variable or dependency, not an expression",
        ));
    }
    let mut reader = Reader {
        chars: text.chars().collect(),
        at: 0,
    };
    let mut values = Vec::new();
    while reader.space()? {
        let command = reader.word();
        if command.is_empty() {
            return Err(invalid("expected a CMake command name"));
        }
        reader.space()?;
        if reader.take() != Some('(') {
            return Err(invalid(format!(
                "expected '(' after CMake command {command}"
            )));
        }
        let args = reader.arguments()?;
        if variable.is_some()
            && command.eq_ignore_ascii_case("set")
            && args.first().is_some_and(|arg| arg == name)
        {
            // CACHE and PARENT_SCOPE affect storage, not the selected literal.
            let scalar = args.len() == 2
                || (args.len() == 3 && args[2] == "PARENT_SCOPE")
                || (args.get(2).is_some_and(|arg| arg == "CACHE")
                    && (args.len() == 5 || (args.len() == 6 && args[5] == "FORCE")));
            if !scalar {
                return Err(invalid(format!(
                    "set({name} ...) must contain one literal value"
                )));
            }
            values.push(args[1].clone());
        } else if variable.is_none()
            && command.eq_ignore_ascii_case("FetchContent_Declare")
            && args
                .first()
                .is_some_and(|arg| arg.eq_ignore_ascii_case(name))
        {
            let urls = args
                .iter()
                .enumerate()
                .filter(|(_, arg)| *arg == "URL")
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if urls.len() != 1 {
                return Err(invalid(format!(
                    "FetchContent_Declare({name} ...) must contain exactly one URL field"
                )));
            }
            let index = urls[0];
            let value = args
                .get(index + 1)
                .ok_or_else(|| invalid(format!("{name}: URL has no value")))?;
            if !(value.starts_with("https://") || value.starts_with("http://"))
                || value.contains(';')
                || value.contains('\\')
                || value.chars().any(char::is_whitespace)
                || args
                    .get(index + 2)
                    .is_some_and(|arg| !URL_OPTIONS.contains(&arg.as_str()))
            {
                return Err(invalid(format!(
                    "{name}: URL must be one literal HTTP(S) archive URL, not a list or expression"
                )));
            }
            values.push(value.clone());
        }
    }
    if values.len() != 1 {
        return Err(invalid(format!(
            "expected exactly one CMake declaration for {name}, found {}; check the name/path or use pattern for nonliteral metadata",
            values.len()
        )));
    }
    let value = values.remove(0);
    if value.is_empty() || (variable.is_some() && (value.contains('$') || value.contains(';'))) {
        return Err(invalid(format!(
            "{name} must have one nonempty literal value; CMake is read, never executed"
        )));
    }
    Ok(value)
}

// Recognized field boundaries after the single URL; additional mirrors are
// intentionally unsupported rather than silently choosing the first.
const URL_OPTIONS: &[&str] = &[
    "URL_HASH",
    "URL_MD5",
    "DOWNLOAD_NAME",
    "DOWNLOAD_NO_EXTRACT",
    "DOWNLOAD_EXTRACT_TIMESTAMP",
    "DOWNLOAD_DIR",
    "DOWNLOAD_NO_PROGRESS",
    "TIMEOUT",
    "INACTIVITY_TIMEOUT",
    "HTTP_USERNAME",
    "HTTP_PASSWORD",
    "HTTP_HEADER",
    "TLS_VERIFY",
    "TLS_VERSION",
    "TLS_CAINFO",
    "NETRC",
    "NETRC_FILE",
    "SOURCE_DIR",
    "BINARY_DIR",
    "SOURCE_SUBDIR",
    "PATCH_COMMAND",
    "UPDATE_COMMAND",
    "UPDATE_DISCONNECTED",
    "CONFIGURE_COMMAND",
    "BUILD_COMMAND",
    "INSTALL_COMMAND",
    "TEST_COMMAND",
    "SYSTEM",
    "EXCLUDE_FROM_ALL",
    "FIND_PACKAGE_ARGS",
    "OVERRIDE_FIND_PACKAGE",
];

struct Reader {
    chars: Vec<char>,
    at: usize,
}

impl Reader {
    fn take(&mut self) -> Option<char> {
        let value = self.chars.get(self.at).copied();
        self.at += usize::from(value.is_some());
        value
    }

    fn bracket(&mut self) -> Result<Option<String>> {
        if self.chars.get(self.at) != Some(&'[') {
            return Ok(None);
        }
        let start = self.at;
        let mut end = start + 1;
        while self.chars.get(end) == Some(&'=') {
            end += 1;
        }
        if self.chars.get(end) != Some(&'[') {
            return Ok(None);
        }
        let equals = end - start - 1;
        self.at = end + 1;
        // CMake ignores a newline immediately following a bracket opener.
        if self.chars.get(self.at) == Some(&'\r') && self.chars.get(self.at + 1) == Some(&'\n') {
            self.at += 2;
        } else if self.chars.get(self.at) == Some(&'\n') {
            self.at += 1;
        }
        let content = self.at;
        while self.at < self.chars.len() {
            if self.chars[self.at] == ']'
                && (0..equals).all(|offset| self.chars.get(self.at + 1 + offset) == Some(&'='))
                && self.chars.get(self.at + equals + 1) == Some(&']')
            {
                let value = self.chars[content..self.at].iter().collect();
                self.at += equals + 2;
                return Ok(Some(value));
            }
            self.at += 1;
        }
        Err(invalid("unterminated CMake bracket argument/comment"))
    }

    fn space(&mut self) -> Result<bool> {
        loop {
            match self.chars.get(self.at) {
                Some(c) if c.is_whitespace() => self.at += 1,
                Some('#') => {
                    self.at += 1;
                    if self.bracket()?.is_none() {
                        while self.chars.get(self.at).is_some_and(|c| *c != '\n') {
                            self.at += 1;
                        }
                    }
                }
                other => return Ok(other.is_some()),
            }
        }
    }

    fn word(&mut self) -> String {
        let start = self.at;
        while self
            .chars
            .get(self.at)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            self.at += 1;
        }
        self.chars[start..self.at].iter().collect()
    }

    fn escaped(&mut self) -> Result<Option<char>> {
        match self.take() {
            Some('\n') => Ok(None),
            Some('\r') if self.chars.get(self.at) == Some(&'\n') => {
                self.at += 1;
                Ok(None)
            }
            Some('n') => Ok(Some('\n')),
            Some('r') => Ok(Some('\r')),
            Some('t') => Ok(Some('\t')),
            Some(c) => Ok(Some(c)),
            None => Err(invalid("unterminated CMake escape")),
        }
    }

    fn escape_into(&mut self, value: &mut String) -> Result<()> {
        // Preserve non-expanding dollars so selectors cannot mistake an escaped
        // literal for a CMake variable reference.
        if self.chars.get(self.at) == Some(&'$') {
            value.push('\\');
        }
        if let Some(c) = self.escaped()? {
            value.push(c);
        }
        Ok(())
    }

    fn arguments(&mut self) -> Result<Vec<String>> {
        let mut args = Vec::new();
        let mut depth = 1;
        while self.space()? {
            match self.chars[self.at] {
                ')' => {
                    self.at += 1;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(args);
                    }
                    args.push(")".into());
                }
                '(' => {
                    self.at += 1;
                    depth += 1;
                    args.push("(".into());
                }
                '"' => {
                    self.at += 1;
                    let mut value = String::new();
                    loop {
                        match self.take() {
                            Some('"') => break,
                            Some('\\') => self.escape_into(&mut value)?,
                            Some(c) => value.push(c),
                            None => return Err(invalid("unterminated quoted CMake argument")),
                        }
                    }
                    args.push(value);
                }
                _ => {
                    if let Some(value) = self.bracket()? {
                        args.push(value.replace('$', "\\$"));
                        continue;
                    }
                    let mut value = String::new();
                    while self
                        .chars
                        .get(self.at)
                        .is_some_and(|c| !c.is_whitespace() && !"()#".contains(*c))
                    {
                        match self.take().expect("checked above") {
                            '\\' => self.escape_into(&mut value)?,
                            c => value.push(c),
                        }
                    }
                    args.push(value);
                }
            }
        }
        Err(invalid("unterminated CMake command"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(selector: &str) -> FileMatch {
        toml::from_str(&format!("path = 'CMakeLists.txt'\n{selector}")).unwrap()
    }

    #[test]
    fn variables_handle_whitespace_comments_quotes_brackets_cache_and_scope() {
        let rule = rule("cmake-variable = 'REV'");
        for text in [
            "set(REV abc123)",
            "# set(REV wrong)\n SeT (\n REV \"abc123\" # comment\n)",
            "#[=[ set(REV wrong) ]=]\nset(REV [=[abc123]=])",
            "set(REV abc123 CACHE STRING \"description\" FORCE)",
            "set(REV abc123 PARENT_SCOPE)",
            "if((TRUE))\nset(REV \"abc\\\n123\")\nendif()",
        ] {
            assert_eq!(select(text, &rule).unwrap(), "abc123", "{text}");
        }
    }

    #[test]
    fn named_fetchcontent_url_ignores_other_dependencies_and_comment_decoys() {
        let rule = rule("cmake-url = 'child'");
        let text = r#"
FetchContent_Declare(other URL https://example.test/other.zip)
#[[ FetchContent_Declare(child URL https://wrong.test/file.zip) ]]
FETCHCONTENT_DECLARE(Child
 # URL https://wrong.test/file.zip
 URL "https://example.test/archive/${REV}.zip"
 URL_HASH SHA256=123
)
"#;
        assert_eq!(
            select(text, &rule).unwrap(),
            "https://example.test/archive/${REV}.zip"
        );
    }

    #[test]
    fn missing_ambiguous_dynamic_malformed_and_list_values_fail() {
        let variable = rule("cmake-variable = 'REV'");
        for text in [
            "set(OTHER abc)",
            "set(REV abc)\nset(REV def)",
            "set(REV)",
            "set(REV a b)",
            "set(REV \"a;b\")",
            "set(REV \"${OTHER}\")",
            "set(REV \"$<IF:TRUE,a,b>\")",
            "set(REV \"unterminated)",
            "set(REV [=[unterminated)",
            "set(REV abc",
            "set(REV abc CACHE)",
            "set(REV abc PARENT_SCOPE garbage)",
            "#[[unterminated",
        ] {
            assert!(select(text, &variable).is_err(), "{text}");
        }
        let url = rule("cmake-url = 'child'");
        for text in [
            "FetchContent_Declare(other URL https://example.test/a.zip)",
            "FetchContent_Declare(child GIT_REPOSITORY https://example.test/repo)",
            "FetchContent_Declare(child URL)",
            "FetchContent_Declare(child URL ${URL})",
            "FetchContent_Declare(child URL https://example.test/a.zip https://example.test/b.zip)",
            "FetchContent_Declare(child URL https://example.test/a.zip URL https://example.test/b.zip)",
            "FetchContent_Declare(child URL https://example.test/a.zip mirror.zip)",
            r#"FetchContent_Declare(child URL "https://example.test/\${REV}.zip")"#,
            "FetchContent_Declare(child URL [[https://example.test/${REV}.zip]])",
        ] {
            assert!(select(text, &url).is_err(), "{text}");
        }
    }
}
