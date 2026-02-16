use std::collections::HashSet;

use burngate::session::{extract_address, is_domain_accepted, parse_command};

// -- parse_command --

#[test]
fn parse_command_ehlo() {
    let (cmd, args) = parse_command("EHLO mail.example.com");
    assert_eq!(cmd, "EHLO");
    assert_eq!(args, "mail.example.com");
}

#[test]
fn parse_command_no_args() {
    let (cmd, args) = parse_command("QUIT");
    assert_eq!(cmd, "QUIT");
    assert_eq!(args, "");
}

#[test]
fn parse_command_with_trailing_whitespace() {
    let (cmd, args) = parse_command("  NOOP  ");
    assert_eq!(cmd, "NOOP");
    assert_eq!(args, "");
}

#[test]
fn parse_command_rcpt_to() {
    let (cmd, args) = parse_command("RCPT TO:<user@example.com>");
    assert_eq!(cmd, "RCPT");
    assert_eq!(args, "TO:<user@example.com>");
}

#[test]
fn parse_command_mail_from() {
    let (cmd, args) = parse_command("MAIL FROM:<sender@example.com> SIZE=1024");
    assert_eq!(cmd, "MAIL");
    assert_eq!(args, "FROM:<sender@example.com> SIZE=1024");
}

#[test]
fn parse_command_empty() {
    let (cmd, args) = parse_command("");
    assert_eq!(cmd, "");
    assert_eq!(args, "");
}

#[test]
fn parse_command_multiple_spaces() {
    let (cmd, args) = parse_command("EHLO   mail.example.com");
    assert_eq!(cmd, "EHLO");
    assert_eq!(args, "mail.example.com");
}

#[test]
fn parse_command_case_insensitive() {
    let (cmd, args) = parse_command("ehlo mail.example.com");
    assert_eq!(cmd, "EHLO");
    assert_eq!(args, "mail.example.com");
}

#[test]
fn parse_command_mixed_case() {
    let (cmd, args) = parse_command("Rcpt TO:<user@example.com>");
    assert_eq!(cmd, "RCPT");
    assert_eq!(args, "TO:<user@example.com>");
}

// -- extract_address --

#[test]
fn extract_address_standard() {
    let addr = extract_address("FROM:<user@example.com>");
    assert_eq!(addr, Some("user@example.com".to_string()));
}

#[test]
fn extract_address_rcpt_to() {
    let addr = extract_address("TO:<recipient@domain.org>");
    assert_eq!(addr, Some("recipient@domain.org".to_string()));
}

#[test]
fn extract_address_empty_brackets() {
    let addr = extract_address("FROM:<>");
    assert_eq!(addr, None);
}

#[test]
fn extract_address_no_brackets() {
    let addr = extract_address("FROM:user@example.com");
    assert_eq!(addr, None);
}

#[test]
fn extract_address_with_params() {
    let addr = extract_address("FROM:<sender@example.com> SIZE=1024");
    assert_eq!(addr, Some("sender@example.com".to_string()));
}

#[test]
fn extract_address_no_at_sign() {
    // Local addresses without @ are valid in SMTP
    let addr = extract_address("TO:<postmaster>");
    assert_eq!(addr, Some("postmaster".to_string()));
}

#[test]
fn extract_address_empty_string() {
    let addr = extract_address("");
    assert_eq!(addr, None);
}

#[test]
fn extract_address_mismatched_brackets() {
    let addr = extract_address("TO:>user@example.com<");
    assert_eq!(addr, None);
}

// -- is_domain_accepted --

fn make_domains(domains: &[&str]) -> HashSet<String> {
    domains.iter().map(|s| s.to_string()).collect()
}

#[test]
fn domain_exact_match() {
    let domains = make_domains(&["tempy.email", "example.com"]);
    assert!(is_domain_accepted("tempy.email", &domains));
    assert!(is_domain_accepted("example.com", &domains));
}

#[test]
fn domain_not_accepted() {
    let domains = make_domains(&["tempy.email"]);
    assert!(!is_domain_accepted("evil.com", &domains));
    assert!(!is_domain_accepted("nottempy.email", &domains));
}

#[test]
fn subdomain_match() {
    let domains = make_domains(&["tempy.email"]);
    assert!(is_domain_accepted("abc123.tempy.email", &domains));
    assert!(is_domain_accepted("sub.tempy.email", &domains));
}

#[test]
fn subdomain_no_match_different_parent() {
    let domains = make_domains(&["tempy.email"]);
    assert!(!is_domain_accepted("abc.evil.com", &domains));
}

#[test]
fn domain_empty() {
    let domains = make_domains(&["tempy.email"]);
    assert!(!is_domain_accepted("", &domains));
}

#[test]
fn domain_no_tld() {
    let domains = make_domains(&["localhost"]);
    assert!(is_domain_accepted("localhost", &domains));
    assert!(!is_domain_accepted("notlocalhost", &domains));
}

#[test]
fn deep_subdomain_no_match() {
    // Only one level of subdomain matching: a.b.tempy.email should match
    // because parent is b.tempy.email, but b.tempy.email is not in accepted.
    // However tempy.email IS accepted, but we only check one level up.
    let domains = make_domains(&["tempy.email"]);
    // a.b.tempy.email -> parent is b.tempy.email -> not in set
    assert!(!is_domain_accepted("a.b.tempy.email", &domains));
    // b.tempy.email -> parent is tempy.email -> in set
    assert!(is_domain_accepted("b.tempy.email", &domains));
}

#[test]
fn domain_case_sensitivity() {
    // The function itself doesn't lowercase - caller is responsible
    let domains = make_domains(&["tempy.email"]);
    assert!(is_domain_accepted("tempy.email", &domains));
    assert!(!is_domain_accepted("TEMPY.EMAIL", &domains));
}

#[test]
fn multiple_domains() {
    let domains = make_domains(&["tempy.email", "jsondb.net", "getemail.live", "mailtemp.xyz"]);
    assert!(is_domain_accepted("tempy.email", &domains));
    assert!(is_domain_accepted("jsondb.net", &domains));
    assert!(is_domain_accepted("getemail.live", &domains));
    assert!(is_domain_accepted("mailtemp.xyz", &domains));
    assert!(is_domain_accepted("sub.tempy.email", &domains));
    assert!(!is_domain_accepted("evil.com", &domains));
}
