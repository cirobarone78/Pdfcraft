//! The smallest window (#1913): at `MIN_WINDOW_SIZE` the mode bar folds the five modes into a menu
//! and keeps the command search reachable, instead of hiding them under the search field.

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_ui_egui::{MIN_WINDOW_SIZE, PdfCraftApp};

fn harness(size: egui::Vec2) -> Harness<'static, PdfCraftApp> {
    let mut h = Harness::builder().with_size(size).build_eframe(|_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app
    });
    h.run_steps(8);
    h
}

#[test]
fn at_the_minimum_width_the_modes_fold_into_a_menu_and_search_stays() {
    let mut h = harness(egui::vec2(MIN_WINDOW_SIZE[0], MIN_WINDOW_SIZE[1]));
    assert!(h.query_by_role_and_label(Role::Button, "Read").is_none(), "no room for five tabs");
    assert!(h.query_by_role_and_label(Role::Button, "Find tools and commands").is_some(), "command search is reachable");
    h.get_by_role_and_label(Role::Button, "All tools").click();
    h.run_steps(4);
    h.get_by_role_and_label(Role::Button, "E-Sign").click();
    h.run_steps(4);
    assert!(h.query_by_role_and_label(Role::Button, "E-Sign").is_some(), "the menu button is named after the mode");
    assert!(h.query_by_role_and_label(Role::Button, "Menu").is_some(), "main menu reachable");
}

#[test]
fn a_wide_window_keeps_the_five_mode_tabs() {
    let h = harness(egui::vec2(1400.0, 900.0));
    for mode in ["All tools", "Read", "Edit", "Convert", "E-Sign"] {
        assert!(h.query_by_role_and_label(Role::Button, mode).is_some(), "{mode} tab");
    }
}
