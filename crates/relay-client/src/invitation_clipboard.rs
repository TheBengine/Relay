//! R2.3: offer a Relay invitation that is already on the clipboard.
//!
//! The clipboard is read at most once per explicit opening of Join friends,
//! only while the invitation field is empty, never per frame or at start-up.
//! Nothing is pasted or joined without a click, and the clipboard text never
//! reaches logs, error messages or preferences (no `Debug`, no `Serialize`).
use crate::invitation::Invitation;

/// Where the text comes from; tests inject a fake so they never read the
/// user's real clipboard.
pub trait ClipboardSource {
    /// `None` for an empty, non-text, locked or unavailable clipboard.
    fn text(&mut self) -> Option<String>;
}

pub struct SystemClipboard;

impl ClipboardSource for SystemClipboard {
    fn text(&mut self) -> Option<String> {
        // Any failure is a normal "nothing to offer"; manual pasting still works.
        arboard::Clipboard::new().ok()?.get_text().ok()
    }
}

/// One join flow's temporary suggestion. A fresh `SetupState` starts armed,
/// so each new opening may check once.
#[derive(Default)]
pub struct ClipboardOffer {
    state: OfferState,
}

#[derive(Default)]
enum OfferState {
    #[default]
    Armed,
    Settled,
    Offered(String),
}

impl ClipboardOffer {
    /// An explicit new opening of Join friends may check again.
    pub fn rearm(&mut self) {
        self.state = OfferState::Armed;
    }

    /// The flow closed or moved on: drop the suggestion and do not check again
    /// until the next explicit opening.
    pub fn settle(&mut self) {
        self.state = OfferState::Settled;
    }

    /// Call while the invitation field is shown. Reads the clipboard only when
    /// armed and the field is empty; typing makes a suggestion disappear.
    pub fn check(&mut self, field: &str, source: &mut dyn ClipboardSource) {
        if !field.trim().is_empty() {
            self.settle();
            return;
        }
        if matches!(self.state, OfferState::Armed) {
            self.state = match source.text() {
                Some(text) if Invitation::parse(&text).is_ok() => OfferState::Offered(text),
                _ => OfferState::Settled,
            };
        }
    }

    pub fn offered(&self) -> bool {
        matches!(self.state, OfferState::Offered(_))
    }

    /// The text read by `check`, for an active click. Never re-reads the clipboard.
    pub fn take(&mut self) -> Option<String> {
        match std::mem::replace(&mut self.state, OfferState::Settled) {
            OfferState::Offered(text) => Some(text),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_setup::SetupState;

    const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

    fn invitation(code: &str) -> String {
        Invitation::new("http://127.0.0.1:4580", SERVER_ID, code)
            .expect("artificial local invitation")
            .link()
    }

    /// Counts reads and returns a fixed answer; never touches the real clipboard.
    struct Fake {
        text: Option<String>,
        reads: usize,
    }

    impl Fake {
        fn new(text: Option<String>) -> Self {
            Self { text, reads: 0 }
        }
    }

    impl ClipboardSource for Fake {
        fn text(&mut self) -> Option<String> {
            self.reads += 1;
            self.text.clone()
        }
    }

    #[test]
    fn fixture_is_a_valid_invitation() {
        assert!(Invitation::parse(&invitation("abc")).is_ok());
    }

    #[test]
    fn browser_invitation_is_offered_once_without_reading_personal_clipboard() {
        let web = Invitation::parse(&invitation("synthetic-code"))
            .unwrap()
            .browser_link();
        let mut source = Fake::new(Some(web.clone()));
        let mut offer = ClipboardOffer::default();
        offer.check("", &mut source);
        offer.check("", &mut source);
        assert!(offer.offered());
        assert_eq!(source.reads, 1);
        assert_eq!(offer.take(), Some(web));
        assert!(offer.take().is_none());
    }

    #[test]
    fn no_read_without_an_opening_and_one_read_per_opening() {
        let mut fake = Fake::new(Some(invitation("abc")));
        // Constructing the state, as at start-up, never reads.
        let mut offer = ClipboardOffer::default();
        assert_eq!(fake.reads, 0);
        for _frame in 0..50 {
            offer.check("", &mut fake);
        }
        assert_eq!(fake.reads, 1, "repainting never polls the clipboard");
        assert!(offer.offered());
        // A settled flow does not read until the next explicit opening.
        offer.settle();
        offer.check("", &mut fake);
        assert_eq!(fake.reads, 1);
        offer.rearm();
        offer.check("", &mut fake);
        offer.check("", &mut fake);
        assert_eq!(fake.reads, 2, "a new opening may check once more");
    }

    #[test]
    fn invalid_empty_or_failed_clipboards_are_quiet() {
        for text in [
            None,
            Some(String::new()),
            Some("hello there".into()),
            Some("https://example.com/join".into()),
            Some(format!("{}{}", invitation("abc"), "x".repeat(5000))),
        ] {
            let mut fake = Fake::new(text);
            let mut offer = ClipboardOffer::default();
            offer.check("", &mut fake);
            offer.check("", &mut fake);
            assert_eq!(fake.reads, 1);
            assert!(!offer.offered());
            assert!(offer.take().is_none());
        }
    }

    #[test]
    fn own_text_is_kept_and_typing_removes_the_offer() {
        // A field with the user's own text is never read over or replaced.
        let mut fake = Fake::new(Some(invitation("abc")));
        let mut offer = ClipboardOffer::default();
        let mut field = String::from("relay://join?my own half-typed text");
        offer.check(&field, &mut fake);
        assert_eq!(fake.reads, 0);
        assert!(!offer.offered());
        field.clear();
        offer.check(&field, &mut fake);
        assert_eq!(fake.reads, 0, "only an opening with an empty field checks");
        // An offer disappears as soon as the user types.
        let mut offer = ClipboardOffer::default();
        offer.check("", &mut fake);
        assert!(offer.offered());
        offer.check("r", &mut fake);
        assert!(!offer.offered());
        assert_eq!(field, "");
    }

    #[test]
    fn using_the_offer_needs_a_click_and_reuses_the_text_read() {
        let mut fake = Fake::new(Some(invitation("first")));
        let mut offer = ClipboardOffer::default();
        offer.check("", &mut fake);
        // Nothing is used until the click takes the text.
        assert!(offer.offered());
        // The clipboard changes before the click; the offered text is used.
        fake.text = Some(invitation("second"));
        let used = offer.take().expect("offered text");
        assert_eq!(Invitation::parse(&used).unwrap().code, "first");
        assert_eq!(fake.reads, 1, "the click does not read again");
        assert!(offer.take().is_none() && !offer.offered());
    }

    #[test]
    fn login_and_join_another_server_keep_separate_offers() {
        let mut fake = Fake::new(Some(invitation("abc")));
        let mut login = SetupState::default();
        let mut community = SetupState::default();
        login.clipboard.check(&login.invitation_text, &mut fake);
        assert!(login.clipboard.offered());
        assert!(!community.clipboard.offered());
        community.clipboard.settle();
        assert!(
            login.clipboard.offered(),
            "closing one flow keeps the other"
        );
        // Resetting a flow, as opening the dialog does, drops its offer.
        login = SetupState::default();
        assert!(!login.clipboard.offered());
    }
}
