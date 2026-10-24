//! One explicit selection for read preparation and Criterion execution.

#[derive(Debug, PartialEq, Eq)]
pub struct ReadSelection {
    case: Option<String>,
    criterion_arguments: Vec<String>,
}

impl ReadSelection {
    pub fn new(case: Option<String>, args: &[String]) -> Result<Self, String> {
        if let Some(id) = &case {
            if args
                .iter()
                .any(|arg| !matches!(arg.as_str(), "--bench" | "--noplot"))
            {
                return Err(
                    "PLAMENU_BENCH_READ_CASE cannot be combined with Criterion arguments \
                     other than --bench/--noplot; use one selection source"
                        .to_owned(),
                );
            }
            let valid = id.split_once('/').is_some_and(|(group, name)| {
                matches!(group, "api" | "web" | "ap")
                    && !name.is_empty()
                    && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            });
            if !valid {
                return Err(
                    "PLAMENU_BENCH_READ_CASE must be one exact api/web/ap case ID".to_owned(),
                );
            }
        }
        Ok(Self {
            case,
            criterion_arguments: args.to_vec(),
        })
    }

    pub fn case(&self) -> Option<&str> {
        self.case.as_deref()
    }

    pub fn criterion_arguments(&self) -> &[String] {
        &self.criterion_arguments
    }

    pub fn includes(&self, group: &str, name: &str) -> bool {
        self.case()
            .is_none_or(|id| id.split_once('/') == Some((group, name)))
    }

    pub fn validate_ids<'a>(
        &self,
        ids: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<(), String> {
        if let Some(id) = &self.case
            && !ids
                .into_iter()
                .any(|(group, name)| self.includes(group, name))
        {
            return Err(format!("unknown focused read case: {id}"));
        }
        Ok(())
    }
}
