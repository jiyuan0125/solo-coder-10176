use std::fmt;
use std::path::Path;

use lazy_regex::{Lazy, lazy_regex};
use regex::Regex;
use serde::Serialize;

use crate::session::Error;

const DEFAULT_PERMUTATIONS_MIN_LEN: usize = 3;
const DEFAULT_PERMUTATIONS_MAX_LEN: usize = 5;
const DEFAULT_PERMUTATIONS_CHARSET: &str = "abcdefghijklmnopqrstuvwxyz0123456789";

static PERMUTATIONS_PARSER: Lazy<Regex> = lazy_regex!(r"^#(\d+)-(\d+)(:.+)?$");
static RANGE_MIN_MAX_PARSER: Lazy<Regex> = lazy_regex!(r"^\[([^\]]+)-([^\]]+)\]$");

const CHINESE_NUMBERS: &[(&str, usize)] = &[
    ("零", 0),
    ("一", 1),
    ("二", 2),
    ("三", 3),
    ("四", 4),
    ("五", 5),
    ("六", 6),
    ("七", 7),
    ("八", 8),
    ("九", 9),
    ("十", 10),
    ("二十", 20),
    ("三十", 30),
    ("四十", 40),
    ("五十", 50),
    ("六十", 60),
    ("七十", 70),
    ("八十", 80),
    ("九十", 90),
    ("一百", 100),
    ("千", 1000),
    ("万", 10000),
];

fn parse_chinese_number(s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }

    if let Ok(n) = s.parse::<usize>() {
        return Some(n);
    }

    for &(name, value) in CHINESE_NUMBERS {
        if s == name {
            return Some(value);
        }
    }

    let mut result = 0;
    let mut temp = 0;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let mut found = false;
        for len in (1..=3).rev() {
            if i + len <= chars.len() {
                let substr: String = chars[i..i + len].iter().collect();
                if let Some(&(_, value)) = CHINESE_NUMBERS.iter().find(|&&(name, _)| name == substr) {
                    if value >= 10 {
                        if temp == 0 {
                            temp = 1;
                        }
                        result += temp * value;
                        temp = 0;
                    } else {
                        temp = value;
                    }
                    i += len;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return None;
        }
    }

    result += temp;

    if result > 0 {
        Some(result)
    } else {
        None
    }
}

fn normalize_separators(s: &str) -> String {
    s.replace('，', ",")
        .replace('、', ",")
        .replace('　', " ")
        .replace('【', "[")
        .replace('】', "]")
}

fn has_nested_brackets(s: &str) -> bool {
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '[' => {
                depth += 1;
                if depth > 1 {
                    return true;
                }
            }
            ']' => {
                depth -= 1;
            }
            _ => {}
        }
    }
    false
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) enum Expression {
    Constant {
        value: String,
    },
    Wordlist {
        filename: String,
    },
    Permutations {
        min: usize,
        max: usize,
        charset: String,
    },
    Range {
        min: usize,
        max: usize,
        set: Vec<usize>,
    },
    Glob {
        pattern: String,
    },
    Multiple {
        expressions: Vec<Expression>,
    },
}

impl Expression {
    pub fn as_string(&self) -> String {
        match self {
            Expression::Constant { value } => value.to_owned(),
            Expression::Wordlist { filename } => filename.to_owned(),
            Expression::Permutations { min, max, charset } => {
                format!("#{min}-{max}:{charset}")
            }
            Expression::Range { min, max, set } => {
                if set.is_empty() {
                    format!("[{min}-{max}]")
                } else {
                    format!(
                        "[{}]",
                        set.iter()
                            .map(|n| n.to_string())
                            .collect::<Vec<String>>()
                            .join(",")
                    )
                }
            }
            Expression::Glob { pattern } => format!("@{pattern}"),
            Expression::Multiple { expressions } => expressions
                .iter()
                .map(|e| e.as_string())
                .collect::<Vec<String>>()
                .join(", "),
        }
    }

    pub fn is_default(&self) -> bool {
        self == &Expression::default()
    }
}

impl Default for Expression {
    fn default() -> Self {
        Expression::Permutations {
            min: DEFAULT_PERMUTATIONS_MIN_LEN,
            max: DEFAULT_PERMUTATIONS_MAX_LEN,
            charset: DEFAULT_PERMUTATIONS_CHARSET.to_owned(),
        }
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Expression::Constant { value } => write!(f, "string '{}'", value),
            Expression::Wordlist { filename } => write!(f, "wordlist {}", filename),
            Expression::Permutations { min, max, charset } => {
                write!(
                    f,
                    "permutations (min:{} max:{} charset:{})",
                    min, max, charset
                )
            }
            Expression::Glob { pattern } => write!(f, "glob {}", pattern),
            Expression::Range { min, max, set } => {
                if set.is_empty() {
                    write!(f, "range {} -> {}", min, max)
                } else {
                    write!(f, "range {:?}", set)
                }
            }
            Expression::Multiple { expressions: _ } => write!(f, "multi {{ ... }}"),
        }
    }
}

pub(crate) fn parse_expression(expr: Option<&String>) -> Result<Expression, Error> {
    if let Some(expr) = expr {
        let first_char = expr.chars().next().unwrap_or(' ');
        let is_at_prefixed = first_char == '@';

        if is_at_prefixed && expr.contains('*') {
            return Ok(Expression::Glob {
                pattern: expr[1..].to_owned(),
            });
        }

        if is_at_prefixed {
            let without_at = &expr[1..];
            let filepath = Path::new(without_at);
            if filepath.exists() && filepath.is_file() {
                return Ok(Expression::Wordlist {
                    filename: without_at.to_owned(),
                });
            }
        }

        match first_char {
            '#' => {
                if let Some(captures) = PERMUTATIONS_PARSER.captures(expr) {
                    let min = captures.get(1).unwrap().as_str().parse::<usize>().map_err(|e| e.to_string())?;
                    let max = captures.get(2).unwrap().as_str().parse::<usize>().map_err(|e| e.to_string())?;
                    if captures.get(3).is_some() {
                        return Ok(Expression::Permutations {
                            min,
                            max,
                            charset: captures
                                .get(3)
                                .unwrap()
                                .as_str()
                                .strip_prefix(':')
                                .unwrap()
                                .to_owned(),
                        });
                    } else {
                        return Ok(Expression::Permutations {
                            min,
                            max,
                            charset: DEFAULT_PERMUTATIONS_CHARSET.to_owned(),
                        });
                    }
                }
            }
            '[' => {
                if expr.ends_with(']') {
                    if has_nested_brackets(expr) {
                        return Err(format!("nested brackets are not allowed: {}", expr));
                    }
                    let inner = &expr[1..expr.len() - 1];
                    if !inner.contains(']') {
                        return parse_bracket_expression(expr);
                    }
                }
            }
            _ => {}
        }

        if expr.contains(',') {
            let multi = expr
                .split(',')
                .map(|s| s.trim().to_owned())
                .collect::<Vec<String>>();
            let mut expressions = vec![];
            for exp in multi {
                expressions.push(parse_expression(Some(&exp))?);
            }

            return Ok(Expression::Multiple { expressions });
        }

        if !is_at_prefixed {
            let filepath = Path::new(&expr);
            if filepath.exists() && filepath.is_file() {
                return Ok(Expression::Wordlist {
                    filename: expr.to_owned(),
                });
            }
        }

        return Ok(Expression::Constant {
            value: expr.to_owned(),
        });
    }

    Ok(Expression::default())
}

fn parse_bracket_expression(expr: &str) -> Result<Expression, Error> {
    if has_nested_brackets(expr) {
        return Err(format!("nested brackets are not allowed: {}", expr));
    }

    let normalized = normalize_separators(expr);

    if let Some(captures) = RANGE_MIN_MAX_PARSER.captures(&normalized) {
        let min_str = captures.get(1).unwrap().as_str().trim();
        let max_str = captures.get(2).unwrap().as_str().trim();

        let min = parse_chinese_number(min_str)
            .ok_or_else(|| format!("invalid range min value: {}", min_str))?;
        let max = parse_chinese_number(max_str)
            .ok_or_else(|| format!("invalid range max value: {}", max_str))?;

        return Ok(Expression::Range {
            min,
            max,
            set: vec![],
        });
    }

    if normalized.starts_with('[') && normalized.ends_with(']') {
        let inner = &normalized[1..normalized.len() - 1];
        if inner.trim().is_empty() {
            return Ok(Expression::Range {
                min: 0,
                max: 0,
                set: vec![],
            });
        }

        let has_comma = inner.contains(',');
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();

        let mut numbers = Vec::new();
        let mut all_valid = true;
        for part in &parts {
            if part.is_empty() {
                continue;
            }
            if let Some(n) = parse_chinese_number(part) {
                numbers.push(n);
            } else {
                all_valid = false;
                break;
            }
        }

        if all_valid && !numbers.is_empty() {
            return Ok(Expression::Range {
                min: 0,
                max: 0,
                set: numbers,
            });
        }

        if has_comma {
            return Err(format!(
                "invalid value in range set expression: {}",
                expr
            ));
        }
    }

    Ok(Expression::Constant {
        value: expr.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_PERMUTATIONS_CHARSET;
    use super::DEFAULT_PERMUTATIONS_MAX_LEN;
    use super::DEFAULT_PERMUTATIONS_MIN_LEN;
    use super::Expression;
    use super::parse_expression;

    #[test]
    fn can_parse_none() {
        let res = parse_expression(None).unwrap();
        assert_eq!(
            res,
            Expression::Permutations {
                min: DEFAULT_PERMUTATIONS_MIN_LEN,
                max: DEFAULT_PERMUTATIONS_MAX_LEN,
                charset: DEFAULT_PERMUTATIONS_CHARSET.to_owned(),
            }
        )
    }

    #[test]
    fn can_parse_constant() {
        let res = parse_expression(Some("admin".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Constant {
                value: "admin".to_owned()
            }
        )
    }

    #[test]
    fn can_parse_filename() {
        #[cfg(unix)]
        let filename = "/etc/hosts";

        #[cfg(windows)]
        let filename = "C:\\Windows\\System32\\drivers\\etc\\hosts";

        let res = parse_expression(Some(filename.to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Wordlist {
                filename: filename.to_owned()
            }
        )
    }

    #[test]
    fn can_parse_constant_with_at() {
        let res = parse_expression(Some("@m_n0t_@_f1l3".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Constant {
                value: "@m_n0t_@_f1l3".to_owned()
            }
        )
    }

    #[test]
    fn can_parse_constant_with_bracket() {
        let res = parse_expression(Some("[m_n0t_@_range]".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Constant {
                value: "[m_n0t_@_range]".to_owned()
            }
        )
    }

    #[test]
    fn can_parse_permutations_with_default_charset() {
        let res = parse_expression(Some("#1-3".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Permutations {
                min: 1,
                max: 3,
                charset: DEFAULT_PERMUTATIONS_CHARSET.to_owned(),
            }
        )
    }

    #[test]
    fn can_parse_permutations_with_custom_charset() {
        let res = parse_expression(Some("#1-10:abcdef".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Permutations {
                min: 1,
                max: 10,
                charset: "abcdef".to_owned(),
            }
        )
    }

    #[test]
    fn can_parse_range_with_min_max() {
        let res = parse_expression(Some("[1-3]".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Range {
                min: 1,
                max: 3,
                set: vec![],
            }
        )
    }

    #[test]
    fn can_parse_range_with_set() {
        let res = parse_expression(Some("[1,3,4, 5, 6, 7, 8, 12,666]".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Range {
                min: 0,
                max: 0,
                set: vec![1, 3, 4, 5, 6, 7, 8, 12, 666],
            }
        )
    }

    #[test]
    fn can_parse_glob() {
        let res = parse_expression(Some("@/etc/*".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Glob {
                pattern: "/etc/*".to_owned()
            }
        )
    }

    #[test]
    fn can_parse_multiple() {
        let expr = "1,[3-5],[6-8],9,[10-13]";
        let res = parse_expression(Some(expr.to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Multiple {
                expressions: vec![
                    Expression::Constant {
                        value: "1".to_string()
                    },
                    Expression::Range {
                        min: 3,
                        max: 5,
                        set: vec![],
                    },
                    Expression::Range {
                        min: 6,
                        max: 8,
                        set: vec![],
                    },
                    Expression::Constant {
                        value: "9".to_string()
                    },
                    Expression::Range {
                        min: 10,
                        max: 13,
                        set: vec![],
                    },
                ]
            }
        )
    }

    #[test]
    fn can_parse_multiple_starting_with_range() {
        let expr = "[1-2],3,4,5,[6-8],9,[10-13]";
        let res = parse_expression(Some(expr.to_owned()).as_ref());
        assert_eq!(
            res,
            Ok(Expression::Multiple {
                expressions: vec![
                    Expression::Range {
                        min: 1,
                        max: 2,
                        set: vec![],
                    },
                    Expression::Constant {
                        value: "3".to_string()
                    },
                    Expression::Constant {
                        value: "4".to_string()
                    },
                    Expression::Constant {
                        value: "5".to_string()
                    },
                    Expression::Range {
                        min: 6,
                        max: 8,
                        set: vec![],
                    },
                    Expression::Constant {
                        value: "9".to_string()
                    },
                    Expression::Range {
                        min: 10,
                        max: 13,
                        set: vec![],
                    },
                ]
            })
        )
    }

    #[test]
    fn can_parse_multiple_with_spaces() {
        let expr = "1, [3-4], 9 ";
        let res = parse_expression(Some(expr.to_owned()).as_ref());
        assert_eq!(
            res,
            Ok(Expression::Multiple {
                expressions: vec![
                    Expression::Constant {
                        value: "1".to_string()
                    },
                    Expression::Range {
                        min: 3,
                        max: 4,
                        set: vec![],
                    },
                    Expression::Constant {
                        value: "9".to_string()
                    },
                ]
            })
        )
    }

    #[test]
    fn at_prefix_glob_with_asterisk() {
        let res = parse_expression(Some("@*.txt".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Glob {
                pattern: "*.txt".to_owned()
            }
        );
    }

    #[test]
    fn at_prefix_wordlist_when_file_exists() {
        let tmpdir = tempfile::tempdir().unwrap();
        let tmppath = tmpdir.path().join("users.txt");
        std::fs::File::create(&tmppath).unwrap();

        let input = format!("@{}", tmppath.to_str().unwrap());
        let res = parse_expression(Some(input).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Wordlist {
                filename: tmppath.to_str().unwrap().to_owned()
            }
        );
    }

    #[test]
    fn at_prefix_constant_when_file_missing() {
        let res = parse_expression(Some("@no-such-file-xyz".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Constant {
                value: "@no-such-file-xyz".to_owned()
            }
        );
    }

    #[test]
    fn nested_brackets_return_error() {
        let res = parse_expression(Some("[[1-3]]".to_owned()).as_ref());
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(err.contains("nested"));
    }

    #[test]
    fn partial_set_parse_failure_returns_error() {
        let res = parse_expression(Some("[1, 2, foo]".to_owned()).as_ref());
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(err.contains("invalid"));
    }

    #[test]
    fn chinese_numbers_in_range() {
        let res = parse_expression(Some("[一-三]".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Range {
                min: 1,
                max: 3,
                set: vec![],
            }
        );
    }

    #[test]
    fn chinese_numbers_in_set() {
        let res = parse_expression(Some("[一, 二, 五]".to_owned()).as_ref()).unwrap();
        assert_eq!(
            res,
            Expression::Range {
                min: 0,
                max: 0,
                set: vec![1, 2, 5],
            }
        );
    }
}
