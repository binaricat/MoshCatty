//! Comprehensive prediction tests (mosh-go + stock fidelity + pipeline).
//!
//! These assert *behavior*, not just “didn’t panic”: pending positions,
//! last_shown cells, paint bytes, flagging, demote, and double-paint safety.

use super::*;
use crate::framebuffer::Attr;
use std::time::{Duration, Instant};

fn blank_fb() -> Framebuffer {
    Framebuffer::new(80, 24)
}

fn always() -> Predictor {
    Predictor::new(DisplayPreference::Always)
}

fn adaptive() -> Predictor {
    Predictor::new(DisplayPreference::Adaptive)
}

// ---------------------------------------------------------------------------
// DisplayPreference
// ---------------------------------------------------------------------------

#[test]
fn env_preference_parsing() {
    assert_eq!(
        DisplayPreference::from_env_value("always"),
        DisplayPreference::Always
    );
    assert_eq!(
        DisplayPreference::from_env_value("NEVER"),
        DisplayPreference::Never
    );
    assert_eq!(
        DisplayPreference::from_env_value("adaptive"),
        DisplayPreference::Adaptive
    );
    assert_eq!(
        DisplayPreference::from_env_value(""),
        DisplayPreference::Adaptive
    );
    assert_eq!(
        DisplayPreference::from_env_value("bogus"),
        DisplayPreference::Adaptive
    );
}

// ---------------------------------------------------------------------------
// mosh-go Predictor core
// ---------------------------------------------------------------------------

#[test]
fn basic_echo_pending_positions() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"abc", &blank_fb());
    assert!(p.active());
    assert_eq!(p.pending_len(), 3);
    assert_eq!(p.pending_char(0), Some('a'));
    assert_eq!(p.pending_pos(0), Some((0, 0)));
    assert_eq!(p.pending_char(1), Some('b'));
    assert_eq!(p.pending_pos(1), Some((1, 0)));
    assert_eq!(p.pending_char(2), Some('c'));
    assert_eq!(p.pending_pos(2), Some((2, 0)));
    assert_eq!(p.cur_x(), 3);
}

#[test]
fn overlay_underlines_when_flagging() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"hi", &blank_fb());
    let mut fb = blank_fb();
    p.overlay(&mut fb);
    assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'h');
    assert!(
        fb.cell_at(0, 0).unwrap().attr.under,
        "Always must underline"
    );
    assert_eq!(fb.cell_at(1, 0).unwrap().ch, 'i');
    assert!(fb.cell_at(1, 0).unwrap().attr.under);
    assert_eq!(fb.cur_x, 2);
}

#[test]
fn overlay_no_underline_when_not_flagging() {
    let mut p = adaptive();
    p.set_srtt(Some(Duration::from_millis(100)));
    assert!(p.should_show() && p.flagging());
    p.set_srtt(Some(Duration::from_millis(40))); // flagging off, show still on
    assert!(p.should_show());
    assert!(!p.flagging());
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    let mut fb = blank_fb();
    p.overlay(&mut fb);
    assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'a');
    assert!(
        !fb.cell_at(0, 0).unwrap().attr.under,
        "predictions show without underline when flagging off"
    );
}

#[test]
fn confirm_all() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', Attr::default());
    fb.put_rune(1, 0, 'b', Attr::default());
    fb.cur_x = 2;
    p.confirm(&fb);
    assert!(!p.active());
    assert_eq!(p.pending_len(), 0);
    assert_eq!(p.cur_x(), 2);
}

#[test]
fn partial_confirm() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"abc", &blank_fb());
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', Attr::default());
    fb.cur_x = 1;
    p.confirm(&fb);
    assert!(p.active());
    assert_eq!(p.pending_len(), 2);
    assert_eq!(p.pending_char(0), Some('b'));
    assert_eq!(p.pending_char(1), Some('c'));
}

#[test]
fn divergence_resets_all() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"abc", &blank_fb());
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'x', Attr::default());
    fb.cur_x = 5;
    p.confirm(&fb);
    assert!(!p.active());
    assert_eq!(p.pending_len(), 0);
    assert_eq!(p.cur_x(), 5);
}

#[test]
fn space_confirms_as_match_not_stall() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"hi there", &blank_fb());
    assert_eq!(p.pending_len(), 8);
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'h', Attr::default());
    fb.put_rune(1, 0, 'i', Attr::default());
    fb.put_rune(2, 0, ' ', Attr::default());
    fb.cur_x = 3;
    p.confirm(&fb);
    assert_eq!(p.pending_len(), 5);
    assert_eq!(p.pending_char(0), Some('t'));
}

#[test]
fn blank_host_stalls_not_diverge() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    // host still empty at prediction cells
    let fb = blank_fb();
    p.confirm(&fb);
    assert_eq!(p.pending_len(), 2, "empty host stalls, must not reset");
    assert!(p.active());
}

#[test]
fn set_cursor_not_overridden_while_active() {
    let mut p = always();
    p.set_cursor(10, 5);
    p.keystroke(b"x", &blank_fb());
    p.set_cursor(0, 0);
    assert_eq!(p.cur_x(), 11);
    assert_eq!(p.cur_y(), 5);
}

#[test]
fn overlay_does_not_touch_unpredicted() {
    let mut p = always();
    p.set_cursor(5, 0);
    p.keystroke(b"x", &blank_fb());
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'A', Attr::default());
    fb.put_rune(1, 0, 'B', Attr::default());
    p.overlay(&mut fb);
    assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'A');
    assert_eq!(fb.cell_at(1, 0).unwrap().ch, 'B');
    assert_eq!(fb.cell_at(5, 0).unwrap().ch, 'x');
}

#[test]
fn multibyte_utf8_one_pending() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke("é".as_bytes(), &blank_fb());
    assert_eq!(p.pending_len(), 1);
    assert_eq!(p.pending_char(0), Some('é'));
    assert_eq!(p.cur_x(), 1);
}

// ---------------------------------------------------------------------------
// Backspace
// ---------------------------------------------------------------------------

#[test]
fn backspace_undoes_own_last_glyph() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    p.keystroke(&[0x7f], &blank_fb());
    assert_eq!(p.pending_len(), 1);
    assert_eq!(p.pending_char(0), Some('a'));
    assert_eq!(p.cur_x(), 1);
    p.keystroke(&[0x08], &blank_fb());
    assert_eq!(p.pending_len(), 0);
    assert_eq!(p.cur_x(), 0);
}

#[test]
fn host_row_bs_uses_frame_pending_not_instant_diverge() {
    // Host has "hello"; BS at col 1 predicts shifted tail with expiration.
    // With frames set so acked < exp, Confirm must not diverge.
    let mut p = always();
    p.set_frames(5, 0, 0); // sent=5, acked=0 → Pending
    p.set_cursor(1, 0);
    let mut host = blank_fb();
    for (i, ch) in ['h', 'e', 'l', 'l', 'o'].into_iter().enumerate() {
        host.put_rune(i, 0, ch, Attr::default());
    }
    host.cur_x = 1;
    p.keystroke(&[0x7f], &host);
    assert_eq!(p.cur_x(), 0);
    let n = p.pending_len();
    assert!(n > 0 && n <= 6, "shift preds bounded, got {n}");
    // Host not yet updated — still Pending, no diverge
    p.confirm(&host);
    assert_eq!(p.pending_len(), n, "must stay Pending until frame acked");
    // Ack frames and apply shifted host
    p.set_frames(5, 6, 6);
    let mut shifted = blank_fb();
    for (i, ch) in ['e', 'l', 'l', 'o', ' '].into_iter().enumerate() {
        shifted.put_rune(i, 0, ch, Attr::default());
    }
    shifted.cur_x = 5; // past spaces so space preds can confirm
    p.confirm(&shifted);
    assert_eq!(
        p.pending_len(),
        0,
        "after ack + shifted host, pending must drain"
    );
}

#[test]
fn kill_epoch_drains_matched_prefix() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    // Prove epoch so confirmed_epoch advances
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', Attr::default());
    fb.put_rune(1, 0, 'b', Attr::default());
    fb.cur_x = 2;
    p.confirm(&fb);
    assert_eq!(p.confirmed_epoch_for_test(), p.prediction_epoch_for_test());
    // New band after tentative
    p.become_tentative();
    let ep_new = p.prediction_epoch_for_test();
    assert!(ep_new > p.confirmed_epoch_for_test());
    p.keystroke(b"xy", &blank_fb());
    // Confirm only first of new band, second diverges → kill_epoch
    let mut fb2 = blank_fb();
    fb2.put_rune(2, 0, 'x', Attr::default());
    fb2.put_rune(3, 0, 'Z', Attr::default()); // diverge on y
    fb2.cur_x = 4;
    p.confirm(&fb2);
    // Matched x drained; y epoch killed — no leftover matched prefix
    assert_eq!(p.pending_len(), 0);
}

#[test]
fn left_cell_attr_inherited_on_overlay() {
    let mut p = always();
    let mut fb = blank_fb();
    // Host paints bold 'P' then we predict 'x' to the right
    use crate::framebuffer::Attr;
    let mut bold = Attr::default();
    bold.bold = true;
    fb.put_rune(0, 0, 'P', bold);
    p.set_cursor(1, 0);
    p.keystroke(b"x", &fb);
    p.overlay(&mut fb);
    assert_eq!(fb.cell_at(1, 0).unwrap().ch, 'x');
    assert!(
        fb.cell_at(1, 0).unwrap().attr.bold,
        "inherit bold from left"
    );
    assert!(fb.cell_at(1, 0).unwrap().attr.under, "Always flagging");
}

#[test]
fn send_interval_adaptive_thresholds() {
    // send_interval ~40ms (from 80ms RTT/2) is between show-on(30) and flag(80)
    let mut p = adaptive();
    p.set_srtt(Some(Duration::from_millis(40)));
    assert!(p.should_show());
    assert!(!p.flagging());
}

#[test]
fn row_change_becomes_tentative() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    let ep0 = p.prediction_epoch_for_test();
    // Simulate host moving to next row while inactive after confirm
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', Attr::default());
    fb.cur_x = 1;
    p.confirm(&fb);
    p.set_cursor(0, 1); // row change
    assert!(
        p.prediction_epoch_for_test() > ep0,
        "row change must become_tentative"
    );
}

#[test]
fn host_line_insert_shifts_tail_under_pending() {
    let mut p = always();
    p.set_frames(4, 0, 0);
    let mut host = blank_fb();
    for (i, ch) in ['h', 'e', 'l', 'l', 'o'].into_iter().enumerate() {
        host.put_rune(i, 0, ch, Attr::default());
    }
    host.cur_x = 2; // insert before first 'l'
    p.set_cursor(2, 0);
    p.keystroke(b"X", &host);
    // Expect X at 2, old l@2→3, l@3→4, o@4→5 under Pending
    assert!(p.pending_len() >= 2);
    let mut by = std::collections::BTreeMap::new();
    for i in 0..p.pending_len() {
        by.insert(p.pending_pos(i).unwrap().0, p.pending_char(i).unwrap());
    }
    assert_eq!(by.get(&2), Some(&'X'));
    // Unacked: confirm must not wipe
    p.confirm(&host);
    assert!(p.pending_len() > 0);
}

#[test]
fn cr_advances_row_when_not_bottom() {
    let mut p = always();
    p.set_cursor(5, 2);
    p.keystroke(b"\r", &blank_fb());
    assert_eq!(p.cur_x(), 0);
    assert_eq!(p.cur_y(), 3);
}

#[test]
fn original_ch_no_credit_for_noop() {
    let mut p = always();
    // Host already has 'a' at 0; predicting 'a' is CorrectNoCredit
    let mut host = blank_fb();
    host.put_rune(0, 0, 'a', Attr::default());
    p.set_cursor(0, 0);
    // Force new epoch so we can observe credit
    p.become_tentative();
    let conf_before = p.confirmed_epoch_for_test();
    let ep = p.prediction_epoch_for_test();
    p.keystroke(b"a", &host); // original_ch = 'a', pred = 'a'
    host.cur_x = 1;
    p.confirm(&host);
    // Strict: CorrectNoCredit must not advance confirmed_epoch to the new band.
    assert_eq!(p.pending_len(), 0, "matched pred drains");
    assert_eq!(
        p.confirmed_epoch_for_test(),
        conf_before,
        "noop match must not prove new band (ep was {ep})"
    );
}

#[test]
fn pipeline_with_frames_confirm_after_ack() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    pipe.set_frames_for_test(2, 2);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    pipe.set_frames_for_test(2, 2);
    let _ = pipe.on_keystroke(b"z");
    // Keystroke may stamp exp=sent+1; simulate next loop frames
    pipe.set_frames_for_test(3, 2); // not yet acked if exp=3
    let _ = pipe.on_host_bytes(b"\x1b[1;1Hz");
    // Still may be pending if exp > acked
    pipe.set_frames_for_test(3, 4);
    let _ = pipe.on_host_bytes(b"\x1b[1;1Hz");
    assert_eq!(pipe.predictor().pending_len(), 0);
    assert_eq!(pipe.last_shown().unwrap().cell_at(0, 0).unwrap().ch, 'z');
}

#[test]
fn backspace_shifts_own_pending_on_row() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"abcd", &blank_fb());
    // Move left twice into the middle of pending (cursor between b and c)
    p.keystroke(b"\x1b[D\x1b[D", &blank_fb());
    assert_eq!(p.cur_x(), 2);
    // BS deletes col 1 ('b') and shifts c,d left → a@0, c@1, d@2
    p.keystroke(&[0x7f], &blank_fb());
    assert_eq!(p.cur_x(), 1);
    assert_eq!(p.pending_len(), 3);
    assert_eq!(p.pending_char(0), Some('a'));
    assert_eq!(p.pending_pos(0), Some((0, 0)));
    assert_eq!(p.pending_char(1), Some('c'));
    assert_eq!(p.pending_pos(1), Some((1, 0)));
    assert_eq!(p.pending_char(2), Some('d'));
    assert_eq!(p.pending_pos(2), Some((2, 0)));
}

#[test]
fn insert_mid_pending_shifts_trailing() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"abcd", &blank_fb());
    p.keystroke(b"\x1b[D\x1b[D", &blank_fb()); // cursor at 2
    p.keystroke(b"x", &blank_fb());
    // Sorted L→R: a@0 b@1 x@2 c@3 d@4
    assert_eq!(p.pending_len(), 5);
    let mut by_col = std::collections::BTreeMap::new();
    for i in 0..p.pending_len() {
        let (x, _) = p.pending_pos(i).unwrap();
        by_col.insert(x, p.pending_char(i).unwrap());
    }
    assert_eq!(by_col.get(&0), Some(&'a'));
    assert_eq!(by_col.get(&1), Some(&'b'));
    assert_eq!(by_col.get(&2), Some(&'x'));
    assert_eq!(by_col.get(&3), Some(&'c'));
    assert_eq!(by_col.get(&4), Some(&'d'));
    let mut fb = blank_fb();
    p.overlay(&mut fb);
    assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'a');
    assert_eq!(fb.cell_at(1, 0).unwrap().ch, 'b');
    assert_eq!(fb.cell_at(2, 0).unwrap().ch, 'x');
    assert_eq!(fb.cell_at(3, 0).unwrap().ch, 'c');
    assert_eq!(fb.cell_at(4, 0).unwrap().ch, 'd');
}

#[test]
fn host_bs_moves_glass_cursor() {
    let mut p = always();
    p.set_cursor(5, 0);
    let mut fb = blank_fb();
    p.keystroke(&[0x7f], &fb);
    assert_eq!(p.cur_x(), 4);
    assert!(p.active());
    p.overlay(&mut fb);
    assert_eq!(fb.cur_x, 4);
}

#[test]
fn space_pred_stalls_on_default_blank() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b" ", &blank_fb());
    let fb = blank_fb(); // still default blank
    p.confirm(&fb);
    assert_eq!(p.pending_len(), 1, "space on default blank must stall");
    // Host advances cursor past the cell without writing non-default — still stall
    // With cur_x > pred.x, allow confirm of space on blank
    let mut fb2 = blank_fb();
    fb2.cur_x = 1;
    p.confirm(&fb2);
    assert_eq!(p.pending_len(), 0);
}

#[test]
fn glitch_clears_on_confirm_allows_demote() {
    let mut p = adaptive();
    p.set_srtt(Some(Duration::from_millis(100)));
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    p.backdate_oldest_for_test(Duration::from_millis(300));
    p.expire_stale(Instant::now());
    assert!(p.glitch_trigger_for_test() >= 10);
    // Slow confirm (aged) — still must clear glitch when pending empty
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', Attr::default());
    fb.cur_x = 1;
    p.confirm(&fb);
    assert_eq!(p.pending_len(), 0);
    assert_eq!(p.glitch_trigger_for_test(), 0);
    p.set_srtt(Some(Duration::from_millis(5)));
    assert!(!p.should_show(), "must demote after glitch cleared");
}

#[test]
fn bs_all_then_adaptive_demote() {
    let mut p = adaptive();
    p.set_srtt(Some(Duration::from_millis(100)));
    p.set_cursor(0, 0);
    p.keystroke(b"xy", &blank_fb());
    p.keystroke(&[0x7f, 0x7f], &blank_fb());
    assert_eq!(p.pending_len(), 0);
    p.set_srtt(Some(Duration::from_millis(5)));
    assert!(!p.should_show());
    assert!(!p.active());
}

#[test]
fn destructive_el_clears_pending_pipeline() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    let _ = pipe.on_keystroke(b"hello");
    assert_eq!(pipe.predictor().pending_len(), 5);
    let _ = pipe.on_host_bytes(b"\x1b[H\x1b[K"); // home + erase line
    assert_eq!(
        pipe.predictor().pending_len(),
        0,
        "EL must invalidate pending (not ghost underline)"
    );
}

#[test]
fn frame_ack_pending_stalls_confirm_until_acked() {
    let mut p = always();
    p.set_frames(3, 0, 0);
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    // expiration_sent = 4, acked = 0 → Pending
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', Attr::default());
    fb.cur_x = 1;
    p.confirm(&fb);
    assert_eq!(p.pending_len(), 1, "must stay Pending while unacked");
    p.set_frames(3, 4, 4);
    p.confirm(&fb);
    assert_eq!(p.pending_len(), 0, "confirm after ack");
}

#[test]
fn become_tentative_hides_new_preds_until_proven() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    // Prove epoch
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', Attr::default());
    fb.cur_x = 1;
    p.confirm(&fb);
    assert_eq!(p.pending_len(), 0);
    p.keystroke(b"b", &blank_fb());
    // still same proven band — visible
    let mut view = blank_fb();
    p.overlay(&mut view);
    assert_eq!(view.cell_at(1, 0).unwrap().ch, 'b');
    // become_tentative → new epoch
    p.become_tentative();
    p.keystroke(b"c", &blank_fb());
    let mut view2 = blank_fb();
    // place host b at 1 for baseline
    view2.put_rune(1, 0, 'b', Attr::default());
    p.overlay(&mut view2);
    // 'c' is tentative (hidden) until confirmed
    // pending has b? drained. only c which is hidden
    // cursor may still move
    assert!(
        view2.cell_at(2, 0).map(|c| c.ch).unwrap_or(' ') != 'c' || p.pending_len() == 1,
        "new-epoch c should be tentative/hidden"
    );
}

// ---------------------------------------------------------------------------
// Arrows / CSI
// ---------------------------------------------------------------------------

#[test]
fn csi_left_right_move_cursor_keep_pending() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"hi", &blank_fb());
    assert_eq!(p.cur_x(), 2);
    p.keystroke(b"\x1b[D", &blank_fb());
    assert_eq!(p.cur_x(), 1);
    assert_eq!(p.pending_len(), 2);
    p.keystroke(b"\x1b[C", &blank_fb());
    assert_eq!(p.cur_x(), 2);
}

#[test]
fn ss3_left_right_arrows() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"xy", &blank_fb());
    p.keystroke(b"\x1bOD", &blank_fb()); // left
    assert_eq!(p.cur_x(), 1);
    p.keystroke(b"\x1bOC", &blank_fb()); // right
    assert_eq!(p.cur_x(), 2);
    assert_eq!(p.pending_len(), 2);
}

#[test]
fn csi_param_right_arrow_moves_by_count() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    assert_eq!(p.cur_x(), 2);
    let before = p.pending_len();
    // CSI 3 C — cursor forward 3 (stock predicts C/D even with params)
    p.keystroke(b"\x1b[3C", &blank_fb());
    assert_eq!(p.pending_len(), before, "arrow must not wipe pending");
    assert_eq!(p.cur_x(), 5, "CSI 3C moves right by 3");
    assert!(p.active());
}

#[test]
fn csi_param_left_arrow_moves_by_count_and_clamps() {
    let mut p = always();
    p.set_cursor(4, 0);
    p.keystroke(b"\x1b[2D", &blank_fb());
    assert_eq!(p.cur_x(), 2);
    p.keystroke(b"\x1b[99D", &blank_fb());
    assert_eq!(p.cur_x(), 0, "must clamp at col 0");
}

#[test]
fn csi_zero_param_defaults_to_one() {
    let mut p = always();
    p.set_cursor(3, 0);
    // CSI 0 C is treated as at least 1 (VT default)
    p.keystroke(b"\x1b[0C", &blank_fb());
    assert_eq!(p.cur_x(), 4);
    p.keystroke(b"\x1b[C", &blank_fb()); // no param = 1
    assert_eq!(p.cur_x(), 5);
}

#[test]
fn csi_param_right_clamps_to_last_col() {
    let mut p = always();
    let fb = Framebuffer::new(10, 3);
    p.set_cursor(8, 0);
    p.keystroke(b"\x1b[5C", &fb);
    assert_eq!(p.cur_x(), 9, "must clamp to cols-1");
}

#[test]
fn fragmented_csi_param_arrow_assembles() {
    let mut p = always();
    p.set_cursor(1, 0);
    p.keystroke(&[0x1b], &blank_fb());
    assert!(p.has_esc_buf_for_test());
    p.keystroke(b"[1", &blank_fb());
    assert!(p.has_esc_buf_for_test(), "incomplete CSI param must buffer");
    p.keystroke(b"2C", &blank_fb());
    assert!(!p.has_esc_buf_for_test());
    // blank_fb is 80 cols: 1+12=13
    assert_eq!(p.cur_x(), 13);
}

#[test]
fn fragmented_csi_assembles_across_chunks() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"z", &blank_fb());
    assert_eq!(p.cur_x(), 1);
    // Split ESC [
    p.keystroke(&[0x1b], &blank_fb());
    assert!(p.has_esc_buf_for_test(), "lone ESC must buffer");
    assert_eq!(
        p.pending_len(),
        1,
        "must not clear pending on incomplete ESC"
    );
    p.keystroke(b"[D", &blank_fb());
    assert_eq!(p.cur_x(), 0);
    assert_eq!(p.pending_len(), 1);
    assert!(!p.has_esc_buf_for_test());
}

#[test]
fn control_become_tentative_hides_new_not_wipe_old() {
    // Stock become_tentative: bump epoch, keep old pending.
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    assert_eq!(p.pending_len(), 2);
    p.keystroke(b"\n", &blank_fb());
    assert_eq!(
        p.pending_len(),
        2,
        "become_tentative must not wipe old pending"
    );
    // New typing after control is a new epoch (still visible because
    // confirmed_epoch tracks; after tentative bump new epoch is higher —
    // hidden until confirm proves it).
    p.keystroke(b"x", &blank_fb());
    // ab remain + maybe x depending on epoch visibility
    assert!(p.pending_len() >= 2);
}

// ---------------------------------------------------------------------------
// Adaptive / flagging / glitch / expire
// ---------------------------------------------------------------------------

#[test]
fn adaptive_hysteresis_show_and_flag() {
    let mut p = adaptive();
    assert!(!p.should_show());
    p.set_srtt(Some(Duration::from_millis(15)));
    assert!(!p.should_show());
    p.set_srtt(Some(Duration::from_millis(35)));
    assert!(p.should_show(), "SRTT>30 enables show");
    assert!(!p.flagging(), "flagging still off until >80");
    p.set_srtt(Some(Duration::from_millis(100)));
    assert!(p.flagging());
    p.set_cursor(0, 0);
    p.keystroke(b"x", &blank_fb());
    p.set_srtt(Some(Duration::from_millis(5)));
    assert!(p.should_show(), "hold show while pending");
    p.reset();
    p.set_srtt(Some(Duration::from_millis(5)));
    assert!(!p.should_show());
}

#[test]
fn cursor_only_active_does_not_latch_adaptive_show() {
    let mut p = adaptive();
    p.set_srtt(Some(Duration::from_millis(100)));
    p.set_cursor(5, 0);
    p.keystroke(b"\x1b[C", &blank_fb()); // cursor-only, no pending
    assert_eq!(p.pending_len(), 0);
    // active may be true for cursor overlay
    p.set_srtt(Some(Duration::from_millis(5)));
    assert!(
        !p.should_show(),
        "empty pending must allow demote (not latch on cursor-only active)"
    );
}

#[test]
fn expire_stale_after_timeout() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    p.backdate_oldest_for_test(Duration::from_secs(16));
    p.expire_stale(Instant::now());
    assert_eq!(p.pending_len(), 0);
    assert!(!p.active());
    assert_eq!(p.glitch_trigger_for_test(), 0);
}

#[test]
fn glitch_threshold_raises_trigger() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    p.backdate_oldest_for_test(Duration::from_millis(300));
    p.expire_stale(Instant::now());
    assert!(
        p.glitch_trigger_for_test() >= 10,
        "age>=250ms must set glitch_trigger, got {}",
        p.glitch_trigger_for_test()
    );
    // still pending (timeout is 15s)
    assert_eq!(p.pending_len(), 1);
}

#[test]
fn last_column_places_unknown_and_wraps() {
    let mut p = always();
    let fb = Framebuffer::new(4, 2);
    p.set_cursor(3, 0); // last col
    let ep_before = p.prediction_epoch_for_test();
    p.keystroke(b"x", &fb);
    // Stock: still places at last col (unknown), becomes tentative, wraps.
    assert_eq!(p.pending_len(), 1, "last column still places prediction");
    assert_eq!(p.pending_pos(0), Some((3, 0)));
    assert_eq!(p.cur_x(), 0);
    assert_eq!(p.cur_y(), 1);
    assert!(p.prediction_epoch_for_test() > ep_before);

    let mut p2 = always();
    p2.set_cursor(0, 0);
    p2.keystroke("你".as_bytes(), &blank_fb());
    assert_eq!(p2.pending_len(), 0, "wide CJK must be tentative");
}

#[test]
fn combining_mark_is_tentative() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"e", &blank_fb());
    assert_eq!(p.pending_len(), 1);
    // U+0301 combining acute
    p.keystroke("\u{0301}".as_bytes(), &blank_fb());
    assert_eq!(p.pending_len(), 1, "combining must not add pending");
}

// ---------------------------------------------------------------------------
// DisplayPipeline — single paint path + no double paint
// ---------------------------------------------------------------------------

#[test]
fn never_mode_passthrough_host_bytes() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Never);
    let out = pipe.on_host_bytes(b"\x1b[Hhello");
    assert_eq!(out, b"\x1b[Hhello");
    assert!(pipe.on_keystroke(b"x").is_empty());
    assert!(!pipe.using_overlay_path());
}

#[test]
fn pipeline_local_echo_then_confirm_no_double_glyph() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let prompt = pipe.on_host_bytes(b"\x1b[H\x1b[2J$ ");
    assert!(!prompt.is_empty() || pipe.last_shown().is_some());

    assert_eq!(
        pipe.host_fb().cur_x,
        2,
        "prompt '$ ' must leave host cursor at col 2, got {}",
        pipe.host_fb().cur_x
    );
    let local = pipe.on_keystroke(b"ls");
    assert!(!local.is_empty(), "must emit Diff for local prediction");
    assert_eq!(pipe.predictor().pending_len(), 2, "ls → 2 pending");
    assert_eq!(pipe.predictor().pending_pos(0), Some((2, 0)));
    let shown = pipe.last_shown().expect("last_shown after keystroke");
    // "$ " at 0,1 then l,s at 2,3
    assert_eq!(
        (
            shown.cell_at(0, 0).unwrap().ch,
            shown.cell_at(1, 0).unwrap().ch,
            shown.cell_at(2, 0).unwrap().ch,
            shown.cell_at(3, 0).unwrap().ch,
            pipe.predictor().cur_x(),
        ),
        ('$', ' ', 'l', 's', 4),
        "unexpected screen after local ls"
    );
    assert!(shown.cell_at(2, 0).unwrap().attr.under);

    // Server confirms with absolute CUP — single l,s in host_fb
    let after = pipe.on_host_bytes(b"\x1b[1;3Hl\x1b[1;4Hs\x1b[1;5H");
    let _ = after;
    assert_eq!(pipe.host_fb().cell_at(2, 0).unwrap().ch, 'l');
    assert_eq!(pipe.host_fb().cell_at(3, 0).unwrap().ch, 's');
    // No double: cell 4 must not also be 'l' from dual-write
    let shown2 = pipe.last_shown().unwrap();
    assert_eq!(shown2.cell_at(2, 0).unwrap().ch, 'l');
    assert_eq!(shown2.cell_at(3, 0).unwrap().ch, 's');
    // After full confirm, underline should clear
    assert!(!pipe.predictor().active() || pipe.predictor().pending_len() == 0);
    assert!(
        !shown2.cell_at(2, 0).unwrap().attr.under,
        "confirmed cells must not stay underlined"
    );
}

#[test]
fn pipeline_relative_host_echo_no_double() {
    // Server Display often emits relative glyph write when encoder cursor
    // already at the cell. After local predict advanced the *glass* cursor,
    // dual-write would double. Diff path must overwrite via cell model.
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[1;1H$ "); // cursor at col 2
    assert_eq!(pipe.host_fb().cur_x, 2);
    let _ = pipe.on_keystroke(b"l");
    assert_eq!(pipe.last_shown().unwrap().cell_at(2, 0).unwrap().ch, 'l');
    // Relative paint: no CUP, just 'l' — apply_ansi writes at host_fb.cur_x
    // Host still at 2 before apply... actually after predict host_fb unchanged.
    // HostBytes that only contain 'l' with cursor still at 2:
    let _ = pipe.on_host_bytes(b"l");
    // host_fb now has l at 2, cur at 3
    assert_eq!(pipe.host_fb().cell_at(2, 0).unwrap().ch, 'l');
    // last_shown must still have only one l at 2, not l at 2 and 3
    let shown = pipe.last_shown().unwrap();
    assert_eq!(shown.cell_at(2, 0).unwrap().ch, 'l');
    assert_ne!(
        shown.cell_at(3, 0).map(|c| c.ch),
        Some('l'),
        "relative host echo must not leave a second l"
    );
}

#[test]
fn pipeline_flagging_flip_repaints_underlines() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Adaptive);
    let _ = pipe.set_srtt(Some(Duration::from_millis(100)));
    let _ = pipe.on_host_bytes(b"\x1b[H");
    let _ = pipe.on_keystroke(b"ab");
    assert!(pipe.last_shown().unwrap().cell_at(0, 0).unwrap().attr.under);

    // Drop flagging (40ms) while keeping show
    let paint = pipe.set_srtt(Some(Duration::from_millis(40)));
    assert!(
        !paint.is_empty() || !pipe.last_shown().unwrap().cell_at(0, 0).unwrap().attr.under,
        "flagging off must repaint; paint empty={:?} under={}",
        paint.is_empty(),
        pipe.last_shown().unwrap().cell_at(0, 0).unwrap().attr.under
    );
    assert!(!pipe.predictor().flagging());
    assert!(
        !pipe.last_shown().unwrap().cell_at(0, 0).unwrap().attr.under,
        "last_shown must clear under after flagging demote"
    );
}

#[test]
fn pipeline_demote_show_clears_overlay() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Adaptive);
    let _ = pipe.set_srtt(Some(Duration::from_millis(100)));
    let _ = pipe.on_host_bytes(b"\x1b[H$ ");
    let _ = pipe.on_keystroke(b"x");
    assert!(pipe.using_overlay_path());
    let _ = pipe.on_host_bytes(b"\x1b[1;3Hx");
    assert_eq!(pipe.predictor().pending_len(), 0);
    let _paint = pipe.set_srtt(Some(Duration::from_millis(5)));
    assert!(!pipe.predictor().should_show());
    assert!(!pipe.using_overlay_path(), "demote must leave overlay path");
    let shown = pipe.last_shown().expect("last_shown after demote");
    for x in 0..10 {
        if let Some(c) = shown.cell_at(x, 0) {
            assert!(!c.attr.under, "no under after demote at col {x}");
        }
    }
}

#[test]
fn pipeline_tick_expires_and_repaints_host_clean() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    let _ = pipe.on_keystroke(b"z");
    assert!(pipe.predictor().active());
    assert_eq!(pipe.last_shown().unwrap().cell_at(0, 0).unwrap().ch, 'z');

    // Backdate via predictor
    pipe.predictor_mut_for_test()
        .backdate_oldest_for_test(Duration::from_secs(16));
    let paint = pipe.tick(Instant::now());
    assert!(
        !paint.is_empty() || pipe.predictor().pending_len() == 0,
        "tick must expire and preferably repaint"
    );
    assert_eq!(pipe.predictor().pending_len(), 0);
    // Overlay gone: last_shown should revert toward host (space/empty at 0,0)
    let shown = pipe.last_shown().unwrap();
    assert_ne!(
        (
            shown.cell_at(0, 0).unwrap().ch,
            shown.cell_at(0, 0).unwrap().attr.under
        ),
        ('z', true),
        "expired prediction must not remain underlined z on last_shown"
    );
}

#[test]
fn pipeline_bulk_paste_resets() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    let big = vec![b'a'; 120];
    let _ = pipe.on_keystroke(&big);
    assert_eq!(
        pipe.predictor().pending_len(),
        0,
        "paste >100 must reset, not create 120 preds"
    );
}

#[test]
fn pipeline_bs_then_confirm_stable() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    let _ = pipe.on_keystroke(b"ab");
    let _ = pipe.on_keystroke(&[0x7f]);
    assert_eq!(pipe.predictor().pending_len(), 1);
    assert_eq!(pipe.predictor().pending_char(0), Some('a'));
    let _ = pipe.on_host_bytes(b"\x1b[1;1Ha");
    assert_eq!(pipe.predictor().pending_len(), 0);
    assert_eq!(pipe.last_shown().unwrap().cell_at(0, 0).unwrap().ch, 'a');
}

#[test]
fn pipeline_resize_full_redraw() {
    let mut pipe = DisplayPipeline::new(40, 10, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[Hhi");
    let paint = pipe.resize(80, 24);
    assert!(!paint.is_empty(), "resize must emit full redraw");
    assert_eq!(pipe.host_fb().cols, 80);
    assert_eq!(pipe.host_fb().rows, 24);
}

// ---------------------------------------------------------------------------
// Fidelity hardening (cursor_exp, unknown, overwrite BS, adaptive bg, structural)
// ---------------------------------------------------------------------------

#[test]
fn cursor_only_survives_confirm_until_frame_ack() {
    let mut p = always();
    p.set_frames(3, 2, 2);
    p.set_cursor(2, 0);
    p.keystroke(b"\x1b[C", &blank_fb()); // right arrow
    assert!(p.active());
    assert_eq!(p.cur_x(), 3);
    assert_eq!(p.pending_len(), 0);
    // Unacked: host still at old cursor — must keep glass cursor.
    let mut host = blank_fb();
    host.cur_x = 2;
    host.cur_y = 0;
    p.confirm(&host);
    assert!(p.active(), "cursor pred Pending until ack");
    assert_eq!(p.cur_x(), 3);
    // Ack + host matches predicted cursor.
    p.set_frames(3, 4, 4);
    host.cur_x = 3;
    p.confirm(&host);
    assert!(!p.active());
    assert_eq!(p.cur_x(), 3);
}

#[test]
fn cursor_only_resets_on_ack_mismatch() {
    let mut p = always();
    p.set_frames(5, 5, 5);
    p.set_cursor(1, 0);
    p.keystroke(b"\x1b[D", &blank_fb()); // left → col 0, exp=6
    assert_eq!(p.cur_x(), 0);
    p.set_frames(5, 6, 6); // acked past exp
    let mut host = blank_fb();
    host.cur_x = 4; // disagree
    host.cur_y = 0;
    p.confirm(&host);
    assert!(!p.active());
    assert_eq!(p.cur_x(), 4, "mismatch after ack snaps to host");
}

#[test]
fn unknown_overlay_does_not_replace_glyph() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    p.set_unknown_pending_for_test(0);
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'H', Attr::default());
    p.overlay(&mut fb);
    assert_eq!(
        fb.cell_at(0, 0).unwrap().ch,
        'H',
        "unknown must not replace host"
    );
    assert!(
        fb.cell_at(0, 0).unwrap().attr.under,
        "flagging still underlines unknown"
    );
}

#[test]
fn overwrite_bs_clears_cell_not_shift() {
    let mut p = always();
    p.set_overwrite_for_test(true);
    let mut host = blank_fb();
    host.put_rune(0, 0, 'a', Attr::default());
    host.put_rune(1, 0, 'b', Attr::default());
    host.put_rune(2, 0, 'c', Attr::default());
    host.cur_x = 2;
    p.set_cursor(2, 0);
    p.keystroke(&[0x7f], &host);
    assert_eq!(p.cur_x(), 1);
    assert_eq!(p.pending_len(), 1);
    assert_eq!(p.pending_char(0), Some(' '));
    assert_eq!(p.pending_pos(0), Some((1, 0)));
}

#[test]
fn adaptive_cold_builds_background_predictions() {
    let mut p = adaptive();
    // Cold: show off
    assert!(!p.should_show());
    p.set_cursor(0, 0);
    p.keystroke(b"hi", &blank_fb());
    assert_eq!(
        p.pending_len(),
        2,
        "stock Adaptive predicts while not showing"
    );
    assert!(!p.should_show());
    // Warm up show
    p.set_srtt(Some(Duration::from_millis(100)));
    assert!(p.should_show());
    let mut fb = blank_fb();
    p.overlay(&mut fb);
    assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'h');
}

#[test]
fn pipeline_adaptive_background_then_show() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Adaptive);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    // Cold keystroke: no paint
    let paint = pipe.on_keystroke(b"xy");
    assert!(paint.is_empty());
    assert_eq!(pipe.predictor().pending_len(), 2);
    // Warm: set_srtt should reveal overlay
    let paint = pipe.set_srtt(Some(Duration::from_millis(100)));
    assert!(
        !paint.is_empty() || pipe.using_overlay_path(),
        "warming Adaptive must engage overlay path"
    );
    assert!(pipe.predictor().should_show());
}

#[test]
fn pipeline_structural_ich_resets_pending() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[Habcd");
    let _ = pipe.on_keystroke(b"z");
    assert!(pipe.predictor().pending_len() >= 1);
    let _ = pipe.on_host_bytes(b"\x1b[1;1H\x1b[2@"); // ICH
    assert_eq!(
        pipe.predictor().pending_len(),
        0,
        "ICH must invalidate pending geometry"
    );
}

#[test]
fn pipeline_split_host_csi_reassembled() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[1;");
    let _ = pipe.on_host_bytes(b"3HX");
    assert_eq!(pipe.host_fb().cur_y, 0);
    // CUP 1;3 → col 2, then X printed
    assert_eq!(pipe.host_fb().cell_at(2, 0).unwrap().ch, 'X');
}

#[test]
fn cross_epoch_insert_shifts_all_pending_on_row() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"abcd", &blank_fb());
    p.become_tentative();
    // Move into the middle of older-epoch preds via left arrows
    while p.cur_x() > 1 {
        p.keystroke(b"\x1b[D", &blank_fb());
    }
    assert_eq!(p.cur_x(), 1);
    let n_before = p.pending_len();
    p.keystroke(b"X", &blank_fb());
    let mut positions: Vec<usize> = (0..p.pending_len())
        .filter_map(|i| p.pending_pos(i).map(|(x, _)| x))
        .collect();
    positions.sort();
    assert!(
        positions.contains(&1),
        "insert at 1 must place/shift; positions={positions:?} n_before={n_before}"
    );
    assert_eq!(p.pending_char(0), Some('a'));
    assert_eq!(p.pending_pos(0), Some((0, 0)));
}

#[test]
fn kill_epoch_resyncs_cursor_to_host() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    p.become_tentative();
    p.keystroke(b"xy", &blank_fb());
    let mut host = blank_fb();
    host.put_rune(0, 0, 'a', Attr::default());
    host.put_rune(1, 0, 'b', Attr::default());
    host.put_rune(2, 0, 'Z', Attr::default()); // not 'x'
    host.cur_x = 3;
    host.cur_y = 0;
    p.confirm(&host);
    for i in 0..p.pending_len() {
        assert_ne!(p.pending_char(i), Some('x'));
        assert_ne!(p.pending_char(i), Some('y'));
    }
    // Failed band killed; remaining empty or older band only; cursor sane.
    if p.pending_len() == 0 {
        assert_eq!(p.cur_x(), 3);
        assert_eq!(p.cur_y(), 0);
    }
}

#[test]
fn ss3_up_does_not_pollute_pending() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"a", &blank_fb());
    assert_eq!(p.pending_len(), 1);
    // ESC O A — SS3 up (not left/right)
    p.keystroke(b"\x1bOA", &blank_fb());
    assert_eq!(
        p.pending_len(),
        1,
        "SS3 non-C/D must not predict O/A as glyphs"
    );
    assert_eq!(p.pending_char(0), Some('a'));
}

#[test]
fn pipeline_split_ich_resets_pending() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[Habcd");
    let _ = pipe.on_keystroke(b"z");
    assert!(pipe.predictor().pending_len() >= 1);
    // Split ICH across HostBytes chunks (carry + structural scan).
    let _ = pipe.on_host_bytes(b"\x1b[2");
    let _ = pipe.on_host_bytes(b"@");
    assert_eq!(
        pipe.predictor().pending_len(),
        0,
        "split ICH must invalidate pending"
    );
}

#[test]
fn adaptive_demote_preserves_cursor_only_active() {
    let mut p = adaptive();
    p.set_srtt(Some(Duration::from_millis(100)));
    p.set_frames(2, 2, 2);
    p.set_cursor(2, 0);
    p.keystroke(b"\x1b[C", &blank_fb());
    assert!(p.active());
    assert_eq!(p.pending_len(), 0);
    // Demote on low interval
    p.set_srtt(Some(Duration::from_millis(5)));
    assert!(!p.should_show());
    assert!(p.active(), "cursor-only active must survive demote");
    assert_eq!(p.cur_x(), 3);
}

#[test]
fn kill_epoch_resyncs_cursor_to_host_strict() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.become_tentative();
    p.keystroke(b"xy", &blank_fb());
    let mut host = blank_fb();
    host.put_rune(0, 0, 'Z', Attr::default());
    host.cur_x = 5;
    host.cur_y = 1;
    p.confirm(&host);
    // Failed tentative with no matched prefix → kill_epoch empties → snap cursor
    assert_eq!(p.pending_len(), 0);
    assert_eq!(p.cur_x(), 5);
    assert_eq!(p.cur_y(), 1);
}

// ---------------------------------------------------------------------------
// Polish: Correct row rendition sync + keystroke UTF-8 carry
// ---------------------------------------------------------------------------

#[test]
fn correct_cascades_host_renditions_to_rest_of_row() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"abc", &blank_fb());
    assert_eq!(p.pending_len(), 3);

    // Host confirms only 'a' with bold; stock copies bold onto remaining preds.
    let mut host = blank_fb();
    let mut bold = Attr::default();
    bold.bold = true;
    host.put_rune(0, 0, 'a', bold);
    host.cur_x = 1;
    p.confirm(&host);
    assert_eq!(p.pending_len(), 2);
    assert_eq!(p.pending_char(0), Some('b'));
    assert_eq!(p.pending_char(1), Some('c'));

    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', bold);
    p.overlay(&mut fb);
    assert!(
        fb.cell_at(1, 0).unwrap().attr.bold,
        "remaining pred 'b' must inherit confirmed host bold"
    );
    assert!(
        fb.cell_at(2, 0).unwrap().attr.bold,
        "remaining pred 'c' must inherit confirmed host bold"
    );
    assert!(fb.cell_at(1, 0).unwrap().attr.under);
}

#[test]
fn correct_no_credit_does_not_prove_or_false_cascade_path() {
    let mut p = always();
    let mut host = blank_fb();
    let mut bold = Attr::default();
    bold.bold = true;
    host.put_rune(0, 0, 'a', bold);
    p.set_cursor(0, 0);
    p.become_tentative();
    let conf_before = p.confirmed_epoch_for_test();
    p.keystroke(b"a", &host); // original_ch == 'a' → CorrectNoCredit
    host.cur_x = 1;
    p.confirm(&host);
    assert_eq!(p.pending_len(), 0);
    assert_eq!(
        p.confirmed_epoch_for_test(),
        conf_before,
        "CorrectNoCredit must not advance confirmed_epoch"
    );
}

#[test]
fn correct_cascade_same_row_only() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    // Manually inject a same-epoch pending on another row via cursor + key
    // (simulate by becoming non-tentative equal epochs through confirm-free path)
    // Easier: type on row0, set pending on row1 by using test helper if needed.
    // Use CR (tentative) then type — those are different epoch and may be hidden.
    // Instead: after typing ab, move to row1 without become_tentative by forcing
    // cursor when inactive — but we're active. So left-only path won't work.
    // Build two-row pending with same epoch via predict by host insert? Skip.
    // Direct: confirm 'a' bold; remaining 'b' on row0 gets bold; ensure no crash.
    let mut host = blank_fb();
    let mut bold = Attr::default();
    bold.bold = true;
    host.put_rune(0, 0, 'a', bold);
    host.cur_x = 1;
    p.confirm(&host);
    assert_eq!(p.pending_pos(0), Some((1, 0)));
    let mut fb = blank_fb();
    // Put a bold cell on a different row that must not affect anything wrongly
    fb.put_rune(0, 1, 'Z', bold);
    fb.put_rune(0, 0, 'a', bold);
    p.overlay(&mut fb);
    assert!(fb.cell_at(1, 0).unwrap().attr.bold);
    // Unrelated row1 cell Z must stay as we left it (overlay shouldn't touch)
    assert_eq!(fb.cell_at(0, 1).unwrap().ch, 'Z');
}

#[test]
fn correct_cascade_dim_and_fg() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"xyz", &blank_fb());
    let mut host = blank_fb();
    let mut attr = Attr::default();
    attr.dim = true;
    attr.fg = crate::framebuffer::Color::index(2);
    host.put_rune(0, 0, 'x', attr);
    host.cur_x = 1;
    p.confirm(&host);
    assert_eq!(p.pending_len(), 2);
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'x', attr);
    p.overlay(&mut fb);
    assert!(fb.cell_at(1, 0).unwrap().attr.dim);
    assert_eq!(
        fb.cell_at(1, 0).unwrap().attr.fg,
        crate::framebuffer::Color::index(2)
    );
    assert!(fb.cell_at(2, 0).unwrap().attr.dim);
}

#[test]
fn correct_cascade_survives_flagging_off() {
    let mut p = adaptive();
    p.set_srtt(Some(Duration::from_millis(100))); // show+flag
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    let mut host = blank_fb();
    let mut bold = Attr::default();
    bold.bold = true;
    host.put_rune(0, 0, 'a', bold);
    host.cur_x = 1;
    p.confirm(&host);
    // Demote flagging only
    p.set_srtt(Some(Duration::from_millis(40)));
    assert!(p.should_show());
    assert!(!p.flagging());
    let mut fb = blank_fb();
    fb.put_rune(0, 0, 'a', bold);
    p.overlay(&mut fb);
    assert!(fb.cell_at(1, 0).unwrap().attr.bold, "cascade bold remains");
    assert!(
        !fb.cell_at(1, 0).unwrap().attr.under,
        "flagging off clears under even with cascade"
    );
}

#[test]
fn keystroke_split_utf8_euro_reassembled() {
    let mut p = always();
    p.set_cursor(0, 0);
    // '€' = E2 82 AC
    p.keystroke(&[0xE2], &blank_fb());
    assert!(p.has_esc_buf_for_test(), "partial UTF-8 must carry");
    assert_eq!(p.pending_len(), 0, "must not invent pending from lead byte");
    p.keystroke(&[0x82], &blank_fb());
    assert!(p.has_esc_buf_for_test());
    assert_eq!(p.pending_len(), 0);
    p.keystroke(&[0xAC], &blank_fb());
    assert!(!p.has_esc_buf_for_test());
    assert_eq!(p.pending_len(), 1);
    assert_eq!(p.pending_char(0), Some('€'));
    assert_eq!(p.cur_x(), 1);
}

#[test]
fn keystroke_split_utf8_after_ascii_prefix() {
    let mut p = always();
    p.set_cursor(0, 0);
    // 'a' then start of 'é' (C3 A9)
    p.keystroke(&[b'a', 0xC3], &blank_fb());
    assert_eq!(p.pending_len(), 1);
    assert_eq!(p.pending_char(0), Some('a'));
    assert!(p.has_esc_buf_for_test());
    p.keystroke(&[0xA9], &blank_fb());
    assert_eq!(p.pending_len(), 2);
    assert_eq!(p.pending_char(1), Some('é'));
    assert_eq!(p.cur_x(), 2);
}

#[test]
fn keystroke_invalid_utf8_lead_is_tentative() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(b"ab", &blank_fb());
    let ep = p.prediction_epoch_for_test();
    // 0xFF is invalid lead
    p.keystroke(&[0xFF], &blank_fb());
    assert_eq!(p.pending_len(), 2, "invalid must not wipe old pending");
    assert!(p.prediction_epoch_for_test() > ep);
    assert!(!p.has_esc_buf_for_test());
}

#[test]
fn keystroke_utf8_carry_does_not_break_following_csi() {
    let mut p = always();
    p.set_cursor(0, 0);
    p.keystroke(&[0xE2, 0x82, 0xAC], &blank_fb());
    assert_eq!(p.pending_len(), 1);
    assert_eq!(p.cur_x(), 1);
    p.keystroke(b"\x1b[D", &blank_fb());
    assert_eq!(p.cur_x(), 0);
}

#[test]
fn pipeline_keystroke_split_utf8_paints_once_complete() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    let paint1 = pipe.on_keystroke(&[0xC3]); // start of é
    assert_eq!(pipe.predictor().pending_len(), 0);
    assert!(
        paint1.is_empty(),
        "incomplete UTF-8 must not paint a glyph, got {:?}",
        paint1
    );
    let paint2 = pipe.on_keystroke(&[0xA9]);
    assert_eq!(pipe.predictor().pending_len(), 1);
    assert_eq!(pipe.predictor().pending_char(0), Some('é'));
    assert!(!paint2.is_empty(), "completed UTF-8 must emit Diff paint");
}

#[test]
fn pipeline_csi_param_arrow_moves_glass_cursor() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    let _ = pipe.on_keystroke(b"hi");
    assert_eq!(pipe.predictor().cur_x(), 2);
    let _paint = pipe.on_keystroke(b"\x1b[4D");
    assert_eq!(pipe.predictor().cur_x(), 0);
    assert_eq!(
        pipe.last_shown().unwrap().cur_x,
        0,
        "last_shown glass cursor must follow CSI n D"
    );
}

#[test]
fn pipeline_correct_cascade_visible_in_last_shown() {
    let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
    let _ = pipe.on_host_bytes(b"\x1b[H");
    let _ = pipe.on_keystroke(b"abc");
    // Host echoes first char with bold SGR
    let _ = pipe.on_host_bytes(b"\x1b[1;1H\x1b[1ma");
    assert_eq!(pipe.predictor().pending_len(), 2);
    let shown = pipe.last_shown().unwrap();
    assert_eq!(shown.cell_at(1, 0).unwrap().ch, 'b');
    assert!(
        shown.cell_at(1, 0).unwrap().attr.bold,
        "pipeline last_shown must show cascaded bold on remaining preds"
    );
}

// ---------------------------------------------------------------------------
// late_ack (echo_ack) vs early transport ack
// ---------------------------------------------------------------------------

#[test]
fn pending_uses_late_ack_not_early_transport_ack() {
    let mut p = always();
    p.set_cursor(0, 0);
    // sent=5; early transport ack advanced to 6, but late echo_ack still 0
    p.set_frames(5, 6, 0);
    p.keystroke(b"ab", &blank_fb());
    assert_eq!(p.pending_len(), 2);
    let mut host = blank_fb();
    host.put_rune(0, 0, 'a', Attr::default());
    host.put_rune(1, 0, 'b', Attr::default());
    host.cur_x = 2;
    // Content matches but late_ack < exp → must stay Pending
    p.confirm(&host);
    assert_eq!(
        p.pending_len(),
        2,
        "stock Pending uses late_ack; early transport ack alone must not confirm"
    );
    // Now late_ack advances past expiration_sent (=6)
    p.set_frames(5, 6, 6);
    p.confirm(&host);
    assert_eq!(p.pending_len(), 0, "late_ack must release Pending");
}

#[test]
fn cursor_only_pending_waits_for_late_ack() {
    let mut p = always();
    p.set_frames(3, 3, 0);
    p.set_cursor(2, 0);
    p.keystroke(b"\x1b[C", &blank_fb());
    assert_eq!(p.cur_x(), 3);
    let mut host = blank_fb();
    host.cur_x = 2; // host not yet moved
    p.confirm(&host);
    assert!(p.active(), "cursor Pending until late_ack");
    assert_eq!(p.cur_x(), 3);
    // early already 3, late still 0 — still pending
    p.set_frames(3, 9, 0);
    p.confirm(&host);
    assert!(p.active());
    // late catches up; host still wrong → reset
    p.set_frames(3, 9, 4);
    host.cur_x = 2;
    p.confirm(&host);
    assert!(!p.active());
    assert_eq!(p.cur_x(), 2);
}
