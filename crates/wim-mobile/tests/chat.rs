//! Drives a session over `FakePlayer`: no network, no native engine.

mod common;

use std::sync::Arc;

use common::{answer, library, over, serial};
use wim::player::FakePlayer;
use wim_mobile::BubbleKind;

#[test]
fn play_reaches_the_player_and_pause_follows() {
    let _g = serial();
    let player = Arc::new(FakePlayer::new());
    let s = over(library("play", &["crest"]), player.clone());
    let id = s.submit("play(crest)".into());
    let bubbles = answer(&s, id);
    let loaded = |c: &String| c.starts_with("load_file(") && c.contains("crest");
    assert!(
        player.calls().iter().any(loaded),
        "{:?} {bubbles:?}",
        player.calls()
    );
    let id = s.submit("pause()".into());
    answer(&s, id);
    let calls = player.calls();
    assert!(
        calls.contains(&"set_pause(true)".to_string())
            || calls.contains(&"toggle_pause()".to_string()),
        "{calls:?}"
    );
}

#[test]
fn replies_carry_the_id_of_their_message() {
    let _g = serial();
    let s = over(library("ids", &["crest"]), Arc::new(FakePlayer::new()));
    let first = s.submit("dry(play(crest))".into());
    let second = s.submit("view(queue)".into());
    assert_ne!(first, second);
    let mut bubbles = answer(&s, second);
    bubbles.extend(s.tick());
    for id in [first, second] {
        let own = |b: &&wim_mobile::Bubble| b.reply_to == id;
        let kinds: Vec<_> = bubbles.iter().filter(own).map(|b| b.kind).collect();
        assert!(
            kinds.contains(&BubbleKind::Reply),
            "no reply for {id}: {bubbles:?}"
        );
    }
}

#[test]
fn bad_input_is_an_error_bubble_for_that_message() {
    let _g = serial();
    let s = over(library("bad", &[]), Arc::new(FakePlayer::new()));
    let id = s.submit("frobnicate(".into());
    let bubbles = answer(&s, id);
    let own = bubbles.iter().filter(|b| b.reply_to == id);
    assert!(
        own.into_iter().any(|b| b.kind == BubbleKind::Error),
        "{bubbles:?}"
    );
}

#[test]
fn what_mobile_cannot_do_declines_with_a_message() {
    let _g = serial();
    let s = over(library("decline", &[]), Arc::new(FakePlayer::new()));
    let id = s.submit("mute()".into());
    let bubbles = answer(&s, id);
    let text = bubbles
        .iter()
        .find(|b| b.reply_to == id && b.kind == BubbleKind::Error)
        .map(|b| b.text.as_str());
    assert!(text.is_some_and(|t| t.contains("mobile")), "{bubbles:?}");
}
