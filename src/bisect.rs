//! Guided bisect troubleshooting (design §FR-5 二分排查).
//!
//! Start with the plugins that could be at fault.  Each round disables half
//! of them and the user reports whether DSH still fails.  "Still fails" means
//! the culprit is among the plugins left enabled; "works" means it is among
//! the disabled half.  When one plugin remains it is the suspect.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bisect {
    /// Plugins that may still be at fault.
    pub candidates: Vec<String>,
    /// Plugins disabled for the current round (a subset of `candidates`).
    pub disabled: Vec<String>,
    pub round: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Disable `Bisect::disabled`, start the instance and ask again.
    Test,
    /// The plugin found to cause the failure.
    Found(String),
    /// With every candidate disabled it still fails: not a plugin problem.
    NotPlugins,
}

impl Bisect {
    /// `None` when there is nothing to bisect.
    pub fn start(candidates: Vec<String>) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        let mut bisect = Self {
            candidates,
            disabled: Vec::new(),
            round: 0,
        };
        bisect.next_round();
        Some(bisect)
    }

    fn next_round(&mut self) {
        self.round += 1;
        // With a single candidate, disable it to confirm.
        let half = self.candidates.len().div_ceil(2);
        self.disabled = self.candidates[..half].to_vec();
    }

    /// Record the result of the current round.
    pub fn report(&mut self, still_fails: bool) -> Step {
        let enabled: Vec<String> = self
            .candidates
            .iter()
            .filter(|c| !self.disabled.contains(c))
            .cloned()
            .collect();
        if self.candidates.len() == 1 {
            return if still_fails {
                Step::NotPlugins
            } else {
                Step::Found(self.candidates[0].clone())
            };
        }
        self.candidates = if still_fails {
            enabled
        } else {
            self.disabled.clone()
        };
        if self.candidates.is_empty() {
            return Step::NotPlugins;
        }
        self.next_round();
        Step::Test
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(plugins: &[&str], culprit: Option<&str>) -> (Step, u32) {
        let mut bisect = Bisect::start(plugins.iter().map(|p| p.to_string()).collect()).unwrap();
        loop {
            let fails = culprit.is_some_and(|c| !bisect.disabled.iter().any(|d| d == c));
            match bisect.report(fails) {
                Step::Test => continue,
                step => return (step, bisect.round),
            }
        }
    }

    #[test]
    fn finds_the_culprit_in_log_rounds() {
        let plugins = ["a", "b", "c", "d", "e", "f", "g", "h"];
        for culprit in plugins {
            let (step, rounds) = run(&plugins, Some(culprit));
            assert_eq!(step, Step::Found(culprit.to_owned()));
            assert!(rounds <= 4, "{culprit}: {rounds} rounds");
        }
        assert_eq!(
            run(&["x", "y", "z"], Some("z")).0,
            Step::Found("z".to_owned())
        );
        assert_eq!(
            run(&["only"], Some("only")).0,
            Step::Found("only".to_owned())
        );
    }

    #[test]
    fn reports_when_plugins_are_not_the_cause() {
        let mut bisect = Bisect::start(vec!["a".into(), "b".into()]).unwrap();
        // Fails with "a" disabled, then fails with "b" disabled too.
        assert_eq!(bisect.report(true), Step::Test);
        assert_eq!(bisect.report(true), Step::NotPlugins);
        assert!(Bisect::start(Vec::new()).is_none());
    }
}
