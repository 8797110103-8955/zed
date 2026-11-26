use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::BackgroundExecutor;

pub struct FuzzyVersionMatches {
    candidates: Arc<[StringMatchCandidate]>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl FuzzyVersionMatches {
    pub fn new(versions: impl IntoIterator<Item = String>) -> Self {
        let mut candidates = Vec::new();
        let mut matches = Vec::new();
        for (candidate_id, string) in versions.into_iter().enumerate() {
            candidates.push(StringMatchCandidate::new(candidate_id, &string));
            matches.push(StringMatch {
                candidate_id,
                score: 0.0,
                positions: Vec::new(),
                string,
            });
        }
        Self {
            candidates: candidates.into(),
            matches,
            selected_index: 0,
        }
    }

    pub fn matches(&self) -> &[StringMatch] {
        &self.matches
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn set_selected_index(&mut self, index: usize) {
        self.selected_index = index;
    }

    pub fn selected_match(&self) -> Option<&StringMatch> {
        self.matches.get(self.selected_index)
    }

    pub fn filter(
        &self,
        query: String,
        executor: BackgroundExecutor,
    ) -> impl Future<Output = Vec<StringMatch>> + use<> {
        let candidates = self.candidates.clone();
        async move {
            fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                100,
                &Default::default(),
                executor,
            )
            .await
        }
    }

    pub fn replace_matches(&mut self, matches: Vec<StringMatch>) {
        self.selected_index = self.selected_index.min(matches.len().saturating_sub(1));
        self.matches = matches;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn filtering_preserves_version_ids_and_clamps_selection(cx: &mut TestAppContext) {
        let mut matches =
            FuzzyVersionMatches::new(["v3.1.0", "v2.0.0", "v1.0.0"].map(String::from));
        matches.set_selected_index(2);
        assert_eq!(
            matches.selected_match().map(|entry| entry.candidate_id),
            Some(2)
        );

        let filtered = matches.filter("2".into(), cx.executor()).await;
        matches.replace_matches(filtered);
        assert_eq!(matches.selected_index(), 0);
        assert_eq!(
            matches.selected_match().map(|entry| entry.candidate_id),
            Some(1)
        );

        let filtered = matches.filter("missing".into(), cx.executor()).await;
        matches.replace_matches(filtered);
        assert!(matches.selected_match().is_none());

        let unfiltered = matches.filter(String::new(), cx.executor()).await;
        matches.replace_matches(unfiltered);
        assert_eq!(
            matches
                .matches()
                .iter()
                .map(|entry| entry.candidate_id)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }
}
