//! The planner's exact search-space pruning rules, each with its own
//! switch: a rule leaves out candidates no optimal plan needs, so a plan
//! selected with a rule on costs what it costs with the rule off. Every
//! rule is on by default; the switches exist to verify that (the plan with
//! a rule off is the unpruned solve) and to measure what each rule saves.
//!
//! `asap_planner --search-space-pruning <rule>=off` switches one rule off
//! (`all=off` every rule). Each stage reads the rules it owns.

/// Which pruning rules the planner applies; [`Default`]: every rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchSpacePruning {
    /// Rule 1, Stage 1 Pass 2: a θ another θ of its set does as well is
    /// left out (same readers, same places, no larger in every size field).
    /// Exact while a larger θ never costs less, which a measured
    /// operation-cost table must also respect.
    pub threshold_theta: bool,
    /// Rule 5, Stage 2: a query-time result is offered with $\ell$ =
    /// `OneEvaluation` only when two readers may read it in one evaluation
    /// context (two consumer inputs, a consumer anchored with `@ end()` or
    /// `@ start()`, or a consumer and a query output). Read once, its
    /// `None` form does all the `OneEvaluation` one does at no more cost:
    /// the only `OneEvaluation` medium offered is memory, which holds at
    /// least what `None` holds while its one consumer runs; retention,
    /// raw data on disk and kept results are charged alike to both forms;
    /// and `None` delivers on every context.
    pub one_evaluation_by_readers: bool,
}

impl Default for SearchSpacePruning {
    fn default() -> Self {
        Self {
            threshold_theta: true,
            one_evaluation_by_readers: true,
        }
    }
}

impl SearchSpacePruning {
    /// Each rule's name and switch, in pipeline order.
    fn switches(&mut self) -> Vec<(&'static str, &mut bool)> {
        vec![
            ("threshold_theta", &mut self.threshold_theta),
            (
                "one_evaluation_by_readers",
                &mut self.one_evaluation_by_readers,
            ),
        ]
    }

    /// Switches rule `rule` (a rule's name, or `all` for every rule) on or
    /// off. Err (bad input) naming the known rules when `rule` is none of
    /// them.
    ///
    /// Example: `set("all", false)` turns every rule off.
    pub fn set(&mut self, rule: &str, on: bool) -> Result<(), String> {
        let mut switches = self.switches();
        if rule == "all" {
            for (_, switch) in switches {
                *switch = on;
            }
            return Ok(());
        }
        let known: Vec<&str> = switches.iter().map(|(name, _)| *name).collect();
        match switches.iter_mut().find(|(name, _)| *name == rule) {
            Some((_, switch)) => {
                **switch = on;
                Ok(())
            }
            None => Err(format!(
                "unknown search-space pruning rule {rule} (rules: all{})",
                known
                    .iter()
                    .map(|name| format!(", {name}"))
                    .collect::<String>()
            )),
        }
    }

    /// The rules switched off, in pipeline order.
    pub fn rules_off(&self) -> Vec<&'static str> {
        let mut copy = *self;
        copy.switches()
            .into_iter()
            .filter(|(_, on)| !**on)
            .map(|(name, _)| name)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `all` is accepted, and an unknown rule is refused with the list of
    /// known rules.
    #[test]
    fn all_is_a_rule_and_an_unknown_rule_is_refused() {
        let mut pruning = SearchSpacePruning::default();
        assert_eq!(pruning.set("all", false), Ok(()));
        assert_eq!(
            pruning.rules_off(),
            vec!["threshold_theta", "one_evaluation_by_readers"]
        );
        assert_eq!(pruning.set("all", true), Ok(()));
        assert_eq!(pruning.set("one_evaluation_by_readers", false), Ok(()));
        assert_eq!(pruning.rules_off(), vec!["one_evaluation_by_readers"]);
        let error = pruning.set("no_such_rule", false).unwrap_err();
        assert!(
            error.contains("unknown search-space pruning rule no_such_rule"),
            "{error}"
        );
    }
}
