use std::collections::HashMap;

use crate::event::Change;
use crate::id::{CommentId, HashId, ItemId};
use crate::item::Priority;
use crate::search::{MatchedField, Query, SearchScope, search};
use crate::state::State;
use crate::testing::*;

struct Scene {
    fixture: Fixture,
    state: State,
    texts: HashMap<CommentId, String>,
    now_ms: u64,
}

impl Scene {
    fn new() -> Self {
        let fixture = Fixture::new(1);
        let state = fixture.state();
        Scene {
            fixture,
            state,
            texts: HashMap::new(),
            now_ms: 2_000,
        }
    }

    fn item(&mut self, title: &str, body: &str) -> ItemId {
        self.now_ms += 1;
        let event = self.fixture.apply(
            &mut self.state,
            0,
            self.now_ms,
            create_with_body("task", title, body),
        );
        event.id().into()
    }

    fn comment(&mut self, item: ItemId, text: &str) -> CommentId {
        self.now_ms += 1;
        let event = self
            .fixture
            .apply(&mut self.state, 0, self.now_ms, comment(item, text));
        let id: CommentId = event.id().into();
        self.texts.insert(id, text.to_string());
        id
    }

    fn found(&self, query: &str, scope: SearchScope) -> Vec<ItemId> {
        let query = Query::parse(query).unwrap();
        search(&self.state, &query, &self.texts, scope)
            .iter()
            .map(|hit| hit.item.id())
            .collect()
    }

    fn live(&self, query: &str) -> Vec<ItemId> {
        self.found(query, SearchScope::Live)
    }
}

#[test]
fn a_query_without_words_is_none() {
    assert_eq!(Query::parse("  \t"), None);
}

#[test]
fn every_word_must_occur_in_some_field_case_insensitively() {
    let mut scene = Scene::new();
    let both = scene.item("Read PACKED-refs", "the parser");
    let spread = scene.item("Other", "packed-refs live here");
    scene.comment(spread, "and the Parser too");
    scene.item("Neither", "parser only");
    assert_eq!(scene.live("packed-refs parser"), [both, spread]);
    assert_eq!(scene.live("PARSER packed-REFS"), [both, spread]);
    assert!(scene.live("packed-refs nothing").is_empty());
}

#[test]
fn title_matches_come_before_body_before_comments_then_list_order() {
    let mut scene = Scene::new();
    let in_comment = scene.item("a", "");
    scene.comment(in_comment, "nudges");
    let in_body = scene.item("b", "about nudges");
    let low_title = scene.item("nudges low", "");
    scene.fixture.apply(
        &mut scene.state,
        0,
        3_000,
        edit(low_title, Change::Priority(Some(Priority::P3))),
    );
    let in_title = scene.item("nudges", "");
    let urgent_title = scene.item("urgent nudges", "");
    scene.fixture.apply(
        &mut scene.state,
        0,
        3_001,
        edit(urgent_title, Change::Priority(Some(Priority::P0))),
    );
    assert_eq!(
        scene.live("nudges"),
        [urgent_title, in_title, low_title, in_body, in_comment]
    );
}

#[test]
fn matches_name_their_field_and_highlight_the_words() {
    let mut scene = Scene::new();
    let item = scene.item(
        "Nudges",
        "first line\nsecond has Packed-Refs and nudges\nthird",
    );
    let comment = scene.comment(item, "  a nudge? no: NUDGES");
    let query = Query::parse("nudges packed-refs").unwrap();
    let hits = search(&scene.state, &query, &scene.texts, SearchScope::Live);
    let matches = &hits[0].matches;
    assert_eq!(matches.len(), 3);
    assert_eq!(matches[0].field, MatchedField::Title);
    assert_eq!(matches[0].context, "Nudges");
    assert_eq!(matches[0].highlights, vec![0..6]);
    assert_eq!(matches[1].field, MatchedField::Body);
    assert_eq!(matches[1].context, "second has Packed-Refs and nudges");
    assert_eq!(matches[1].highlights, vec![11..22, 27..33]);
    assert!(matches!(
        &matches[2].field,
        MatchedField::Comment { id, .. } if *id == comment
    ));
    assert_eq!(matches[2].context, "a nudge? no: NUDGES");
    assert_eq!(matches[2].highlights, vec![13..19]);
}

#[test]
fn a_long_line_is_cropped_around_the_first_word() {
    let mut scene = Scene::new();
    let long = format!("{} needle {}", "x".repeat(200), "y".repeat(200));
    scene.item("t", &long);
    let query = Query::parse("needle").unwrap();
    let hits = search(&scene.state, &query, &scene.texts, SearchScope::Live);
    let found = &hits[0].matches[0];
    assert!(found.context.starts_with('…') && found.context.ends_with('…'));
    assert!(found.context.chars().count() <= 122);
    let highlighted = &found.context[found.highlights[0].clone()];
    assert_eq!(highlighted, "needle");
}

#[test]
fn an_id_or_its_prefix_matches_the_item_and_comes_first() {
    let mut scene = Scene::new();
    let mentioned = scene.item("mentions it", "");
    let named = scene.item("plain", "");
    let id = named.to_base32();
    scene.comment(mentioned, &format!("see #{}", &id[..6]));
    assert_eq!(scene.live(&format!("#{}", &id[..6])), [named, mentioned]);
    assert_eq!(scene.live(&id[..5]), [named, mentioned]);
    assert!(scene.live(&format!("#{} nothing", &id[..6])).is_empty());
    assert_eq!(scene.live(&format!("#{} plain", &id[..6])), [named]);
}

#[test]
fn archived_items_are_searched_on_request_and_come_last_in_their_tier() {
    let mut scene = Scene::new();
    let old = scene.item("nudges old", "");
    scene
        .fixture
        .apply(&mut scene.state, 0, 3_000, archive(old));
    let live = scene.item("nudges live", "");
    assert_eq!(scene.live("nudges"), [live]);
    assert_eq!(
        scene.found("nudges", SearchScope::LiveAndArchived),
        [live, old]
    );
}

#[test]
fn comments_without_a_loaded_text_do_not_match() {
    let mut scene = Scene::new();
    let item = scene.item("t", "");
    let comment = scene.comment(item, "secret");
    scene.texts.remove(&comment);
    assert!(scene.live("secret").is_empty());
}
