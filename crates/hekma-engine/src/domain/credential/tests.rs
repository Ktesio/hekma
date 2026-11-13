//! The credential module's test seams and unit tests (story 22-1).

use std::cell::{Cell, RefCell};

use super::*;

thread_local! {
    static RNG_FAILS: Cell<bool> = const { Cell::new(false) };
    static FORCED_ID: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Make every mint on this thread fail (or stop failing).
pub(crate) fn fail_rng(fail: bool) {
    RNG_FAILS.with(|cell| cell.set(fail));
}

pub(crate) fn rng_fails() -> bool {
    RNG_FAILS.with(Cell::get)
}

/// Force every token id derived on this thread to `id` (or stop).
pub(crate) fn force_token_id(id: Option<&str>) {
    FORCED_ID.with(|cell| *cell.borrow_mut() = id.map(str::to_string));
}

pub(crate) fn forced_token_id() -> Option<String> {
    FORCED_ID.with(|cell| cell.borrow().clone())
}

#[test]
fn a_mint_is_the_prefix_and_52_crockford_characters() {
    for class in TokenClass::ALL {
        let token = mint(class).unwrap();
        let text = token.secret().expose_secret();
        assert!(text.starts_with(class.prefix()), "{class:?}");
        assert_eq!(text.len(), PREFIX_LEN + TOKEN_BODY_LEN);
        assert!(text[PREFIX_LEN..].bytes().all(is_body_char));
        // It parses back as its own class.
        let (parsed, secret) = parse_presented(text).unwrap();
        assert_eq!(parsed, class);
        assert!(hash_presented(&secret).ct_eq(&token.hash()));
        // The token id is the first 12 hex of the hash.
        let hex: String = crate::sha256::digest(text.as_bytes())[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(token.token_id(), hex);
        assert_eq!(token.token_id().len(), TOKEN_ID_LEN);
    }
}

#[test]
fn ten_thousand_mints_never_collide() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..10_000 {
        let token = mint(TokenClass::RunKey).unwrap();
        assert!(seen.insert(*token.hash().as_bytes()));
    }
}

#[test]
fn the_encoder_is_big_endian_crockford_with_trailing_zero_padding() {
    assert_eq!(encode_body(&[0u8; 32]), "0".repeat(52));
    let all = encode_body(&[0xFF; 32]);
    // 256 one-bits then four zero pad bits: 51 × `Z` and a final `G`
    // (0b10000).
    assert_eq!(&all[..51], "Z".repeat(51));
    assert_eq!(&all[51..], "G");
    let mut one = [0u8; 32];
    one[0] = 0b0000_1000;
    assert!(encode_body(&one).starts_with("1"));
}

#[test]
fn a_failing_random_source_mints_nothing_and_names_itself() {
    fail_rng(true);
    let err = mint(TokenClass::RunKey).unwrap_err();
    fail_rng(false);
    assert!(err.to_string().contains("random source failed"), "{err}");
    assert!(mint(TokenClass::RunKey).is_ok());
}

#[test]
fn debug_never_shows_a_token() {
    let key = mint(TokenClass::RunKey).unwrap();
    let refresh = mint(TokenClass::Refresh).unwrap();
    let key_text = key.secret().expose_secret().to_string();
    let refresh_text = refresh.secret().expose_secret().to_string();
    let minted = format!("{key:?}");
    assert!(!minted.contains(&key_text) && minted.contains(&key.token_id()));
    let issued = IssuedCredentials::new(RunId::from_wire("run_x"), key, refresh);
    let rendered = format!("{issued:?}");
    assert!(!rendered.contains(&key_text) && !rendered.contains(&refresh_text));
    let (_, parsed) = parse_presented(&key_text).unwrap();
    assert!(!format!("{parsed:?}").contains(&key_text));
    let hash = TokenHash::of(&key_text);
    assert_eq!(
        format!("{hash:?}"),
        format!("TokenHash({})", hash.token_id())
    );
    let scan = scan_url(&format!("/v0/runs/{key_text}"));
    assert!(!format!("{scan:?}").contains(&key_text));
}

#[test]
fn a_presentation_must_have_the_exact_public_shape() {
    let key = mint(TokenClass::RunKey).unwrap();
    let text = key.secret().expose_secret();
    assert!(parse_presented(text).is_some());
    assert!(parse_presented(&text[..text.len() - 1]).is_none());
    assert!(parse_presented(&format!("{text}0")).is_none());
    assert!(parse_presented(&text.to_ascii_lowercase()).is_none());
    assert!(parse_presented(&text.replace("hk_run_", "hk_xyz_")).is_none());
    let mut bad = text.to_string();
    bad.replace_range(PREFIX_LEN..PREFIX_LEN + 1, "U");
    assert!(parse_presented(&bad).is_none(), "U is not Crockford");
    assert!(parse_presented("").is_none());
    assert!(parse_presented("hk_run_").is_none());
    assert_eq!(
        TokenClass::of_prefix("hk_ann_x"),
        Some(TokenClass::Announce)
    );
    assert_eq!(TokenClass::of_prefix("nope"), None);
}

#[test]
fn hashes_compare_by_content_only() {
    let a = TokenHash::of("a");
    let b = TokenHash::of("b");
    assert!(a.ct_eq(&TokenHash::of("a")));
    assert!(!a.ct_eq(&b));
    assert_eq!(TokenHash::from_bytes(*a.as_bytes()), a);
}

#[test]
fn a_url_scan_finds_tokens_raw_encoded_and_in_any_case() {
    let key = mint(TokenClass::RunKey).unwrap();
    let text = key.secret().expose_secret().to_string();
    let hash = key.hash();
    let found = |url: &str| {
        let scan = scan_url(url);
        let complete = scan
            .tokens
            .iter()
            .any(|token| hash_presented(token).ct_eq(&hash));
        (scan.found, complete)
    };
    assert_eq!(found(&format!("/v0/runs/{text}")), (true, true));
    assert_eq!(found(&format!("/v0/runs?key={text}")), (true, true));
    assert_eq!(
        found(&format!("/v0/runs?key={}", text.to_ascii_lowercase())),
        (true, true)
    );
    assert_eq!(
        found(&format!("/v0/runs?key={}", text.replace('_', "%5F"))),
        (true, true)
    );
    assert_eq!(
        found(&format!("/v0/runs?key={}", text.replace('_', "%255F"))),
        (true, true)
    );
    // A truncated token is still refused, but names no complete token.
    assert_eq!(found(&format!("/v0/runs/{}", &text[..20])), (true, false));
    assert_eq!(found("/v0/runs/HK_REF_ABCDEFGH"), (true, false));
    // A real token behind a prefix-shaped decoy is still found, whole: a
    // short body, a one-character body, and a token run straight into
    // another (the first body ends in `HK`, the start of the next prefix).
    for (case, decoyed) in [
        format!("/v0/runs/hk_run_{text}"),
        format!("/v0/runs/hk_run_X{text}"),
        format!("/v0/runs/HK_REF_hk_ann_{text}"),
        format!("/v0/runs/{text}{text}"),
    ]
    .into_iter()
    .enumerate()
    {
        // The case number, never the token, names a failure.
        assert_eq!(found(&decoyed), (true, true), "decoy case {case}");
    }
    let other = mint(TokenClass::Refresh).unwrap();
    let both = scan_url(&format!(
        "/v0/runs/{text}{}",
        other.secret().expose_secret()
    ));
    assert!(both
        .tokens
        .iter()
        .any(|token| hash_presented(token).ct_eq(&other.hash())));
    // Ordinary paths are not caught.
    assert_eq!(found("/v0/runs/run_01J"), (false, false));
    assert_eq!(found("/v0/instances/hk_run_"), (false, false));
    assert_eq!(found("/v0/instances/hk_run_abc"), (false, false));
    assert_eq!(found(""), (false, false));
    assert_eq!(percent_decode("a%2"), "a%2");
    assert_eq!(percent_decode("%zz%41"), "%zzA");
}

#[test]
fn a_token_packed_url_keeps_at_most_the_cap_without_duplicates() {
    // Many distinct tokens, each repeated, packed end to end (the pre-auth
    // amplifier shape): the scan keeps the FIRST distinct tokens, in order,
    // up to the cap, with no duplicates, and still reports `found`.
    let distinct: Vec<MintedToken> = (0..URL_SCAN_MAX_TOKENS * 3)
        .map(|_| mint(TokenClass::RunKey).unwrap())
        .collect();
    let mut url = String::from("/v0/");
    for token in &distinct {
        for _ in 0..4 {
            url.push_str(token.secret().expose_secret());
        }
    }
    let scan = scan_url(&url);
    assert!(scan.found);
    assert_eq!(scan.tokens.len(), URL_SCAN_MAX_TOKENS);
    let hashes: Vec<TokenHash> = scan.tokens.iter().map(hash_presented).collect();
    for (index, hash) in hashes.iter().enumerate() {
        assert!(
            !hashes[..index].iter().any(|seen| seen.ct_eq(hash)),
            "token {index} is a duplicate"
        );
        assert!(
            hash.ct_eq(&distinct[index].hash()),
            "token {index} is not the {index}th distinct"
        );
    }
    // A URL with one token, repeated, still yields exactly that one.
    let one = distinct[0].secret().expose_secret().repeat(50);
    let scan = scan_url(&format!("/v0/{one}"));
    assert!(scan.found);
    assert_eq!(scan.tokens.len(), 1);
}
