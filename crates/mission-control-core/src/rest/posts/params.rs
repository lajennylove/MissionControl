//! Request parameters as `WP_REST_Request` holds them, and their validation against the
//! controller's schema, as `rest_validate_value_from_schema` and
//! `rest_sanitize_value_from_schema` check them: a value of the wrong type, out of range or not
//! one of the allowed values makes the request fail with `rest_invalid_param` (400), listing
//! every bad parameter at once.

use serde_json::{Map, Value, json};

use crate::front::decode;
use crate::rest::RestError;

/// Request parameters: names to JSON values. A query string or a form body gives strings,
/// arrays (`a[]=1&a[]=2`) and objects (`a[x]=1`); a JSON body gives any JSON.
pub type Params = Map<String, Value>;

/// How many `name=value` pairs a query string or form body is read for, as PHP's default
/// `max_input_vars`. Later pairs are ignored.
pub const MAX_INPUT_VARS: usize = 1000;

/// Parses a query string or an `application/x-www-form-urlencoded` body as PHP fills `$_GET` and
/// `$_POST`: `+` and `%XX` decoded, `a[]=1&a[]=2` an array, `a[x]=1` an object (one level), the
/// last of repeated plain keys winning. At most [`MAX_INPUT_VARS`] pairs are read.
pub fn parse_form(text: &str) -> Params {
    let mut params = Params::new();
    for pair in text
        .trim_start_matches('?')
        .split('&')
        .filter(|pair| !pair.is_empty())
        .take(MAX_INPUT_VARS)
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(key, true);
        let value = Value::String(decode(value, true));
        match key.split_once('[') {
            Some((name, rest)) if !name.is_empty() && rest.ends_with(']') => {
                let inner = &rest[..rest.len() - 1];
                if inner.is_empty() {
                    match params.get_mut(name) {
                        Some(Value::Array(list)) => list.push(value),
                        _ => {
                            params.insert(name.to_owned(), Value::Array(vec![value]));
                        }
                    }
                } else if !inner.contains(['[', ']']) {
                    match params.get_mut(name) {
                        Some(Value::Object(object)) => {
                            object.insert(inner.to_owned(), value);
                        }
                        _ => {
                            let mut object = Map::new();
                            object.insert(inner.to_owned(), value);
                            params.insert(name.to_owned(), Value::Object(object));
                        }
                    }
                }
            }
            _ => {
                params.insert(key, value);
            }
        }
    }
    params
}

/// The parameters that failed validation, gathered so the error names them all.
#[derive(Debug, Default)]
pub(crate) struct Invalid {
    params: Map<String, Value>,
    details: Map<String, Value>,
}

impl Invalid {
    /// Records that `param` failed with WordPress's `code` and `message`.
    pub(crate) fn add(&mut self, param: &str, code: &str, message: String, data: Value) {
        self.details.insert(
            param.to_owned(),
            json!({"code": code, "message": message, "data": data}),
        );
        self.params.insert(param.to_owned(), Value::String(message));
    }

    /// Whether any parameter failed.
    pub(crate) fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    /// `rest_invalid_param` naming every failed parameter, or `Ok` when none failed.
    pub(crate) fn finish(self) -> Result<(), RestError> {
        if self.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = self.params.keys().map(String::as_str).collect();
        let message = format!("Invalid parameter(s): {}", names.join(", "));
        Err(RestError::new("rest_invalid_param", message, 400)
            .with_data("params", Value::Object(self.params))
            .with_data("details", Value::Object(self.details)))
    }

    /// `param` is not of `kind` (`integer`, `boolean`, `string`, `array`, `object`).
    pub(crate) fn wrong_type(&mut self, param: &str, kind: &str) {
        self.add(
            param,
            "rest_invalid_type",
            format!("{param} is not of type {kind}."),
            json!({"param": param}),
        );
    }

    /// `param` is not one of `allowed`.
    pub(crate) fn not_in_enum(&mut self, param: &str, allowed: &[&str]) {
        self.add(
            param,
            "rest_not_in_enum",
            format!("{param} is not one of {}.", list_sentence(allowed)),
            json!({"param": param}),
        );
    }

    /// `param` is outside `min..=max`.
    pub(crate) fn out_of_bounds(&mut self, param: &str, min: i64, max: Option<i64>) {
        let message = match max {
            Some(max) => {
                format!("{param} must be between {min} (inclusive) and {max} (inclusive)")
            }
            None => format!("{param} must be greater than or equal to {min}"),
        };
        self.add(
            param,
            "rest_out_of_bounds",
            message,
            json!({"param": param}),
        );
    }
}

/// `items` as WordPress's `wp_sprintf_l` lists them: `a`, `a and b`, `a, b, and c`.
pub(crate) fn list_sentence(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// An integer as `rest_is_integer` reads one: a JSON integer, a float with no fraction, or a
/// numeric string with no fraction.
pub(crate) fn as_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                .map(|f| f as i64)
        }),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| {
                s.parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15)
                    .map(|f| f as i64)
            })
        }
        _ => None,
    }
}

/// A boolean as `rest_is_boolean` reads one: `true`, `false`, `"true"`, `"false"`, `"1"`, `"0"`,
/// `""`, 1 and 0.
pub(crate) fn as_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        },
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A list as `wp_parse_list` reads one: an array as it is, or a string split on commas and
/// whitespace. Any other value is a list of itself.
pub(crate) fn as_list(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::String(s) => s
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|part| !part.is_empty())
            .map(|part| Value::String(part.to_owned()))
            .collect(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

/// Reads parameters one by one, recording each failure in an [`Invalid`].
pub(crate) struct Reader<'a> {
    params: &'a Params,
    /// The failures so far.
    pub(crate) invalid: Invalid,
}

impl<'a> Reader<'a> {
    /// A reader of `params`.
    pub(crate) fn new(params: &'a Params) -> Self {
        Self {
            params,
            invalid: Invalid::default(),
        }
    }

    /// The raw value of `name`, if given.
    pub(crate) fn raw(&self, name: &str) -> Option<&'a Value> {
        self.params.get(name)
    }

    /// Whether `name` was given.
    pub(crate) fn has(&self, name: &str) -> bool {
        self.params.contains_key(name)
    }

    /// Integer `name`, within `min..=max` when given.
    pub(crate) fn integer(
        &mut self,
        name: &str,
        min: Option<i64>,
        max: Option<i64>,
    ) -> Option<i64> {
        let value = self.params.get(name)?;
        let Some(n) = as_integer(value) else {
            self.invalid.wrong_type(name, "integer");
            return None;
        };
        if min.is_some_and(|min| n < min) || max.is_some_and(|max| n > max) {
            self.invalid
                .out_of_bounds(name, min.unwrap_or(i64::MIN), max);
            return None;
        }
        Some(n)
    }

    /// Boolean `name`.
    pub(crate) fn boolean(&mut self, name: &str) -> Option<bool> {
        let value = self.params.get(name)?;
        let b = as_boolean(value);
        if b.is_none() {
            self.invalid.wrong_type(name, "boolean");
        }
        b
    }

    /// String `name`. A number is not a string, as `rest_validate_value_from_schema` decides.
    pub(crate) fn string(&mut self, name: &str) -> Option<String> {
        match self.params.get(name)? {
            Value::String(s) => Some(s.clone()),
            _ => {
                self.invalid.wrong_type(name, "string");
                None
            }
        }
    }

    /// String `name`, one of `allowed`.
    pub(crate) fn one_of(&mut self, name: &str, allowed: &[&str]) -> Option<String> {
        let value = self.string(name)?;
        if allowed.contains(&value.as_str()) {
            Some(value)
        } else {
            self.invalid.not_in_enum(name, allowed);
            None
        }
    }

    /// A list of integers `name`, each at least `min`; empty when not given.
    pub(crate) fn integers(&mut self, name: &str, min: i64) -> Vec<i64> {
        let Some(value) = self.params.get(name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, item) in as_list(value).iter().enumerate() {
            match as_integer(item) {
                Some(n) if n >= min => out.push(n),
                Some(_) => {
                    self.invalid
                        .out_of_bounds(&format!("{name}[{i}]"), min, None);
                    return Vec::new();
                }
                None => {
                    self.invalid.wrong_type(&format!("{name}[{i}]"), "integer");
                    return Vec::new();
                }
            }
        }
        out
    }

    /// A list of strings `name`; empty when not given.
    pub(crate) fn strings(&mut self, name: &str) -> Vec<String> {
        let Some(value) = self.params.get(name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, item) in as_list(value).into_iter().enumerate() {
            match item {
                Value::String(s) => out.push(s),
                Value::Number(n) => out.push(n.to_string()),
                _ => {
                    self.invalid.wrong_type(&format!("{name}[{i}]"), "string");
                    return Vec::new();
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_read_as_php_reads_them() {
        let params = parse_form("a=1&b%5B%5D=2&b[]=3&c[x]=y&d=one+two&a=4&flag");
        assert_eq!(
            Value::Object(params),
            json!({"a": "4", "b": ["2", "3"], "c": {"x": "y"}, "d": "one two", "flag": ""})
        );
    }

    #[test]
    fn values_coerce_as_wordpress_coerces_them() {
        assert_eq!(as_integer(&json!("12")), Some(12));
        assert_eq!(as_integer(&json!("12.0")), Some(12));
        assert_eq!(as_integer(&json!("12.5")), None);
        assert_eq!(as_integer(&json!(true)), None);
        assert_eq!(as_boolean(&json!("false")), Some(false));
        assert_eq!(as_boolean(&json!("yes")), None);
        assert_eq!(
            as_list(&json!("1, 2 3")),
            [json!("1"), json!("2"), json!("3")]
        );
        assert_eq!(list_sentence(&["asc", "desc"]), "asc and desc");
        assert_eq!(list_sentence(&["a", "b", "c"]), "a, b, and c");
    }

    #[test]
    fn every_bad_parameter_is_named() {
        let params = parse_form("per_page=0&order=up&page=x");
        let mut reader = Reader::new(&params);
        reader.integer("per_page", Some(1), Some(100));
        reader.one_of("order", &["asc", "desc"]);
        reader.integer("page", Some(1), None);
        let error = reader.invalid.finish().unwrap_err();
        assert_eq!(error.code, "rest_invalid_param");
        assert_eq!(error.message, "Invalid parameter(s): order, page, per_page");
        assert_eq!(
            error.data["params"]["per_page"],
            json!("per_page must be between 1 (inclusive) and 100 (inclusive)")
        );
        assert_eq!(
            error.data["details"]["order"]["code"],
            json!("rest_not_in_enum")
        );
    }
}
