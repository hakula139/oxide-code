//! The model settings a session sends with each request, which the engine owns and swaps.

use super::{Effort, ModelSpec, PromptCacheTtl};

/// What a session asks the model for. The effort stays resolved against the model, so a model swap
/// re-resolves it.
#[derive(Debug, Clone)]
pub(crate) struct ModelSettings {
    model: ModelSpec,
    /// `None` for models outside the catalog.
    effort: Option<Effort>,
    /// User-set output cap. `None` defers to [`ModelSpec::resolve_max_tokens`].
    max_tokens: Option<u32>,
    prompt_cache_ttl: PromptCacheTtl,
    /// Whether adaptive thinking opts into summarized text.
    show_thinking: bool,
}

impl ModelSettings {
    /// Settings for `model`, resolving the `effort` pick against it.
    pub(crate) fn new(
        model: ModelSpec,
        effort: Option<Effort>,
        max_tokens: Option<u32>,
        prompt_cache_ttl: PromptCacheTtl,
        show_thinking: bool,
    ) -> Self {
        Self {
            effort: model.resolve_effort(effort),
            model,
            max_tokens,
            prompt_cache_ttl,
            show_thinking,
        }
    }

    /// Settings for `model` with its default effort and output cap, a one-hour cache, and thinking
    /// text hidden.
    #[cfg(test)]
    pub(crate) fn test(model: &str) -> Self {
        Self::new(
            ModelSpec::resolve(model),
            None,
            None,
            PromptCacheTtl::OneHour,
            false,
        )
    }

    pub(crate) fn model(&self) -> &ModelSpec {
        &self.model
    }

    pub(crate) fn effort(&self) -> Option<Effort> {
        self.effort
    }

    /// The output cap that requests to the active model carry.
    pub(crate) fn max_tokens(&self) -> u32 {
        self.model.resolve_max_tokens(self.max_tokens)
    }

    pub(crate) fn prompt_cache_ttl(&self) -> PromptCacheTtl {
        self.prompt_cache_ttl
    }

    pub(crate) fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    /// Swaps the model and re-resolves the effort against it. Thinking, betas, context management,
    /// and the default output cap derive from the model per request.
    pub(crate) fn set_model(&mut self, id: String) {
        let model = ModelSpec::resolve(id);
        self.effort = model.resolve_effort(self.effort);
        self.model = model;
    }

    /// Swaps the effort and returns the one requests now carry, which stays `None` on a model
    /// outside the catalog.
    pub(crate) fn set_effort(&mut self, pick: Effort) -> Option<Effort> {
        self.effort = self.model.resolve_effort(Some(pick));
        self.effort
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(model: &str, effort: Option<Effort>) -> ModelSettings {
        ModelSettings::new(
            ModelSpec::resolve(model),
            effort,
            None,
            PromptCacheTtl::OneHour,
            false,
        )
    }

    mod new {
        use super::*;

        #[test]
        fn resolves_the_effort_pick_against_the_model() {
            for (model, pick, expect) in [
                ("claude-opus-5-5", None, Some(Effort::Medium)),
                ("claude-opus-5-5", Some(Effort::Max), Some(Effort::Max)),
                ("custom-model", Some(Effort::High), None),
            ] {
                let settings = ModelSettings::new(
                    ModelSpec::resolve(model),
                    pick,
                    None,
                    PromptCacheTtl::OneHour,
                    false,
                );

                assert_eq!(settings.effort(), expect, "{model} pick={pick:?}");
            }
        }
    }

    mod max_tokens {
        use super::*;

        #[test]
        fn prefers_the_user_cap_over_the_model_default() {
            for (model, cap, expect) in [
                ("claude-opus-5-5", Some(4_096), 4_096),
                ("claude-opus-5-5", None, 64_000),
                ("custom-model", None, 16_000),
            ] {
                let settings = ModelSettings::new(
                    ModelSpec::resolve(model),
                    None,
                    cap,
                    PromptCacheTtl::FiveMin,
                    false,
                );

                assert_eq!(settings.max_tokens(), expect, "{model} cap={cap:?}");
            }
        }
    }

    mod set_model {
        use super::*;

        #[test]
        fn swaps_the_model_and_re_resolves_the_effort() {
            for (from_model, from_effort, swap_to, expect) in [
                (
                    "claude-opus-5-5",
                    Some(Effort::Xhigh),
                    "claude-sonnet-5-5",
                    Some(Effort::Xhigh),
                ),
                (
                    "custom-model",
                    None,
                    "claude-sonnet-5-5",
                    Some(Effort::High),
                ),
                ("claude-opus-5-5", Some(Effort::High), "custom-model", None),
            ] {
                let mut settings = settings(from_model, from_effort);

                settings.set_model(swap_to.to_owned());

                assert_eq!(settings.model().id(), swap_to, "{from_model} → {swap_to}");
                assert_eq!(settings.effort(), expect, "{from_model} → {swap_to}");
            }
        }
    }

    mod set_effort {
        use super::*;

        #[test]
        fn resolves_the_pick_against_the_active_model() {
            for (model, initial, pick, expect) in [
                (
                    "claude-sonnet-5-5",
                    Some(Effort::High),
                    Effort::Max,
                    Some(Effort::Max),
                ),
                ("custom-model", None, Effort::High, None),
            ] {
                let mut settings = settings(model, initial);

                let resolved = settings.set_effort(pick);

                assert_eq!(resolved, expect, "{model} pick={pick:?}");
                assert_eq!(settings.effort(), expect, "{model} pick={pick:?}: stored");
            }
        }
    }
}
