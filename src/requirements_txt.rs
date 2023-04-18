//! Parses a subset of requirement.txt syntax
//!
//! <https://pip.pypa.io/en/stable/reference/requirements-file-format/>
//!
//! Supported:
//!  * [PEP 508 requirements](https://packaging.python.org/en/latest/specifications/dependency-specifiers/)
//!  * `-r`
//!  * `-c`
//!  * `--hash` (postfix)
//!
//! Explicit error:
//!  * `-e`
//!
//! Unsupported:
//!  * `<path>`. Use `name @ path` instead
//!  * `<archive_url>`. Use `name @ archive_url` instead
//!  * Options without a requirement, such as `--find-links` or `--index-url`
//!
//! Grammar as implemented:
//!
//! ```text
//! file = (statement | empty ('#' any*)? '\n')*
//! empty = whitespace*
//! statement = constraint_include | requirements_include | editable_requirement | requirement
//! constraint_include = '-c' ('=' | wrappable_whitespaces) filepath
//! requirements_include = '-r' ('=' | wrappable_whitespaces) filepath
//! editable_requirement = '-e' ('=' | wrappable_whitespaces) requirement
//! # We check whether the line starts with a letter or a number, in that case we assume it's a
//! # PEP 508 requirement
//! # https://packaging.python.org/en/latest/specifications/name-normalization/#valid-non-normalized-names
//! # This does not (yet?) support plain files or urls, we use a letter or a number as first
//! # character to assume a PEP 508 requirement
//! requirement = [a-zA-Z0-9] pep508_grammar_tail wrappable_whitespaces hashes
//! hashes = ('--hash' ('=' | wrappable_whitespaces) [a-zA-Z0-9-_]+ ':' [a-zA-Z0-9-_] wrappable_whitespaces+)*
//! # This should indicate a single backslash before a newline
//! wrappable_whitespaces = whitespace ('\\\n' | whitespace)*
//! ```

use crate::poetry_integration::poetry_toml;
use anyhow::bail;
use fs_err as fs;
use pep508_rs::{Pep508Error, Requirement, VersionOrUrl};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;
use unscanny::{Pattern, Scanner};

/// Parsed and flattened requirements.txt with requirements and constraints
#[derive(Debug, Deserialize, Clone, Default, Eq, PartialEq, Serialize)]
pub struct RequirementsTxt {
    /// The actual requirements with the hashes
    pub requirements: Vec<RequirementEntry>,
    /// Constraints included with `-c`
    pub constraints: Vec<Requirement>,
}

/// A requirement with additional metadata from the requirements.txt, currently only hashes but in
/// the future also editable an similar information
#[derive(Debug, Deserialize, Clone, Eq, PartialEq, Serialize)]
pub struct RequirementEntry {
    /// The actual PEP 508 requirement
    pub requirement: Requirement,
    /// Hashes of the downloadable packages
    pub hashes: Vec<String>,
    /// Editable installation, see e.g. <https://stackoverflow.com/q/35064426/3549270>
    pub editable: bool,
}

impl RequirementsTxt {
    /// See module level documentation
    pub fn parse(requirements_txt: impl AsRef<Path>) -> Result<Self, RequirementsTxtError> {
        let content = fs::read_to_string(&requirements_txt)?;
        let mut s = Scanner::new(&content);

        let mut requirements_data = RequirementsTxt::default();
        while !s.done() {
            requirements_data.parse_entry(&mut s, &content, &requirements_txt)?;
        }
        Ok(requirements_data)
    }

    /// Parse a single entry, that is a requirement, an inclusion or a comment line
    fn parse_entry(
        &mut self,
        s: &mut Scanner,
        content: &str,
        requirements_txt: &impl AsRef<Path>,
    ) -> Result<(), RequirementsTxtError> {
        // Unwrap: We just read the file, we know it can't be the root or an empty string
        let parent = requirements_txt.as_ref().parent().unwrap();

        s.eat_whitespace();
        if s.eat_if("#") {
            // skip comments
            s.eat_until('\n');
        } else if s.eat_if("-r") {
            let location = s.cursor();
            let requirements_file = parse_value(s, ['\n', '#'], &requirements_txt)?;
            let sub_file = parent.join(requirements_file);
            let sub_requirements =
                Self::parse(&sub_file).map_err(|err| RequirementsTxtError::Subfile {
                    file: requirements_txt.as_ref().to_path_buf(),
                    source: Box::new(err),
                    location,
                })?;
            // Add each to the correct category
            self.requirements.extend(sub_requirements.requirements);
            self.constraints.extend(sub_requirements.constraints);
        } else if s.eat_if("-c") {
            let location = s.cursor();
            let constraint_file = parse_value(s, ['\n', '#'], &requirements_txt)?;
            let sub_file = parent.join(constraint_file);
            let sub_constraints =
                Self::parse(&sub_file).map_err(|err| RequirementsTxtError::Subfile {
                    file: requirements_txt.as_ref().to_path_buf(),
                    source: Box::new(err),
                    location,
                })?;
            // Here we add both to constraints
            self.constraints.extend(
                sub_constraints
                    .requirements
                    .into_iter()
                    .map(|requirement_entry| requirement_entry.requirement),
            );
            self.constraints.extend(sub_constraints.constraints);
        } else if s.eat_if("-e") {
            let (requirement, hashes) =
                parse_requirement_and_hashes(s, &content, &requirements_txt)?;
            self.requirements.push(RequirementEntry {
                requirement,
                hashes,
                editable: true,
            });
        } else if s.at(char::is_ascii_alphanumeric) {
            let (requirement, hashes) =
                parse_requirement_and_hashes(s, &content, &requirements_txt)?;
            self.requirements.push(RequirementEntry {
                requirement,
                hashes,
                editable: false,
            });
        }
        Ok(())
    }

    /// Method to bridge between the new parser and the poetry assumptions of the existing code
    pub fn into_poetry(
        self,
        requirements_txt: &Path,
    ) -> anyhow::Result<BTreeMap<String, poetry_toml::Dependency>> {
        if !self.constraints.is_empty() {
            bail!(
                "Constraints (`-c`) from {} are not supported yet",
                requirements_txt.display()
            );
        }
        let mut poetry_requirements: BTreeMap<String, poetry_toml::Dependency> = BTreeMap::new();
        for requirement_entry in self.requirements {
            let version = match requirement_entry.requirement.version_or_url {
                None => "*".to_string(),
                Some(VersionOrUrl::Url(_)) => {
                    bail!(
                        "Unsupported url requirement in {}: '{}'",
                        requirements_txt.display(),
                        requirement_entry.requirement,
                    )
                }
                Some(VersionOrUrl::VersionSpecifier(specifiers)) => specifiers.to_string(),
            };

            let dep = poetry_toml::Dependency::Expanded {
                version: Some(version),
                optional: Some(false),
                extras: requirement_entry.requirement.extras.clone(),
                git: None,
                branch: None,
            };
            poetry_requirements.insert(requirement_entry.requirement.name, dep);
        }
        Ok(poetry_requirements)
    }
}

/// Eat whitespace and ignore newlines escaped with a backslash
fn eat_wrappable_whitespace<'a>(s: &mut Scanner<'a>) -> &'a str {
    let start = s.cursor();
    s.eat_whitespace();
    // Allow multiple escaped line breaks
    while s.eat_if("\\\n") {
        s.eat_whitespace();
    }
    s.from(start)
}

/// Parse a PEP 508 requirement with optional trailing hashes
fn parse_requirement_and_hashes(
    s: &mut Scanner,
    content: &&str,
    requirements_txt: &impl AsRef<Path>,
) -> Result<(Requirement, Vec<String>), RequirementsTxtError> {
    // PEP 508 requirement
    let start = s.cursor();
    // Termination: s.eat() eventually becomes None
    let (end, has_hashes) = loop {
        let end = s.cursor();

        //  We look for the end of the line ...
        if s.eat_if('\n') {
            break (end, false);
        }
        // ... or`--hash` separated by whitespace ...
        if !(eat_wrappable_whitespace(s)).is_empty() && (s.after()).starts_with("--") {
            break (end, true);
        }
        // ... or the end of the file (after potential whitespace), which works like the end of line
        if s.eat().is_none() {
            break (end, false);
        }
    };
    let requirement = Requirement::from_str(&content[start..end]).map_err(|err| {
        RequirementsTxtError::Pep508 {
            source: err,
            file: requirements_txt.as_ref().to_path_buf(),
            start,
            end,
        }
    })?;
    let hashes = if has_hashes {
        parse_hashes(s, &requirements_txt)?
    } else {
        Vec::new()
    };
    Ok((requirement, hashes))
}

/// Parse `--hash=... --hash ...` after a requirement
fn parse_hashes(
    s: &mut Scanner,
    requirements_txt: &impl AsRef<Path>,
) -> Result<Vec<String>, RequirementsTxtError> {
    let mut hashes = Vec::new();
    if s.eat_while("--hash").is_empty() {
        return Err(RequirementsTxtError::Parser {
            message: format!(
                "Expected '--hash', found '{:?}'",
                s.eat_while(|c: char| !c.is_whitespace())
            ),
            file: requirements_txt.as_ref().to_path_buf(),
            location: s.cursor(),
        });
    }
    let hash = parse_value(s, char::is_whitespace, &requirements_txt)?;
    hashes.push(hash.to_string());
    loop {
        eat_wrappable_whitespace(s);
        if s.eat_while("--hash").is_empty() {
            break;
        }
        let hash = parse_value(s, char::is_whitespace, &requirements_txt)?;
        hashes.push(hash.to_string());
    }
    Ok(hashes)
}

/// In `-<key>=<value>` or `-<key> value`, this parses the part after the key
fn parse_value<'a, T>(
    s: &mut Scanner<'a>,
    until: impl Pattern<T>,
    requirements_txt: impl AsRef<Path>,
) -> Result<&'a str, RequirementsTxtError> {
    if s.eat_if('=') {
        // Explicit equals sign
        Ok(s.eat_until(until).trim_end())
    } else if s.eat_if(char::is_whitespace) {
        // Key and value are separated by whitespace instead
        s.eat_whitespace();
        Ok(s.eat_until(until).trim_end())
    } else {
        Err(RequirementsTxtError::Parser {
            message: format!("Expected '=' or whitespace, found {:?}", s.peek()),
            file: requirements_txt.as_ref().to_path_buf(),
            location: s.cursor(),
        })
    }
}

/// Error parsing requirements.txt
#[derive(Debug, Error)]
pub enum RequirementsTxtError {
    #[error(transparent)]
    IO(#[from] io::Error),
    #[error("{message} in {file} position {location}")]
    Parser {
        message: String,
        file: PathBuf,
        location: usize,
    },
    #[error("Couldn't parse requirement in {file} position {start} to {end}")]
    Pep508 {
        source: Pep508Error,
        file: PathBuf,
        start: usize,
        end: usize,
    },
    #[error("Failed to parse {} position {} due to an error in an included file", file.display(), location)]
    Subfile {
        file: PathBuf,
        source: Box<RequirementsTxtError>,
        location: usize,
    },
}

#[cfg(test)]
mod test {
    use crate::requirements_txt::RequirementsTxt;
    use fs_err as fs;
    use indoc::indoc;
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn test_requirements_txt_parsing() {
        for basic in fs::read_dir(Path::new("test-data").join("requirements-txt")).unwrap() {
            let basic = basic.unwrap().path();
            if basic.extension().unwrap_or_default().to_str().unwrap() != "txt" {
                continue;
            }
            let actual = RequirementsTxt::parse(&basic).unwrap();
            let fixture = basic.with_extension("json");
            // Update the json fixtures
            // fs::write(&fixture, &serde_json::to_string_pretty(&actual).unwrap()).unwrap();
            let snapshot = serde_json::from_str(&fs::read_to_string(fixture).unwrap()).unwrap();
            assert_eq!(actual, snapshot);
        }
    }

    /// Pass test only - currently fails due to `-e ./` in pyproject.toml-constrained.in
    #[test]
    #[ignore]
    fn test_pydantic() {
        for basic in fs::read_dir(Path::new("test-data").join("requirements-pydantic")).unwrap() {
            let basic = basic.unwrap().path();
            if !["txt", "in"].contains(&basic.extension().unwrap_or_default().to_str().unwrap()) {
                continue;
            }
            RequirementsTxt::parse(&basic).unwrap();
        }
    }

    #[test]
    fn test_invalid_include_missing_file() {
        let basic = Path::new("test-data")
            .join("requirements-txt")
            .join("invalid-include");
        let err = RequirementsTxt::parse(&basic).unwrap_err();
        let errors = anyhow::Error::new(err)
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let expected = &[
            "Failed to parse test-data/requirements-txt/invalid-include position 2 due to an error in an included file",
            "failed to open file `test-data/requirements-txt/missing.txt`",
            "No such file or directory (os error 2)"
        ];
        assert_eq!(errors, expected)
    }

    #[test]
    fn test_invalid_requirement() {
        let basic = Path::new("test-data")
            .join("requirements-txt")
            .join("invalid-requirement");
        let err = RequirementsTxt::parse(&basic).unwrap_err();
        let errors = anyhow::Error::new(err)
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let expected = &[
            "Couldn't parse requirement in test-data/requirements-txt/invalid-requirement position 0 to 15",
            indoc! {"
                Expected an alphanumeric character starting the extra name, found 'ö'
                numpy[ö]==1.29
                      ^"
            }
        ];
        assert_eq!(errors, expected)
    }

    #[test]
    fn test_requirements_txt_poetry() {
        let expected = indoc! {r#"
            [inflection]
            version = "==0.5.1"
            optional = false
            
            [numpy]
            version = "*"
            optional = false

            [pandas]
            version = ">=1, <2"
            optional = false
            extras = ["tabulate"]
            
            [upsidedown]
            version = "==0.4"
            optional = false
        "#};

        let path = Path::new("test-data")
            .join("requirements-txt")
            .join("for-poetry.txt");
        let reqs = RequirementsTxt::parse(&path)
            .unwrap()
            .into_poetry(&path)
            .unwrap();
        // sort lines
        let reqs = BTreeMap::from_iter(&reqs);
        let poetry_toml = toml::to_string(&reqs).unwrap();
        assert_eq!(poetry_toml, expected);
    }
}
