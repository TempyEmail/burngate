use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use burngate::ratelimit::{client_key, IpConcurrencyLimiter, IpRateLimiter};

#[tokio::test]
async fn allows_up_to_limit() {
    let limiter = IpRateLimiter::new(3);
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    assert!(limiter.check_and_increment(ip).await);
    assert!(limiter.check_and_increment(ip).await);
    assert!(limiter.check_and_increment(ip).await);
    // 4th should be rejected
    assert!(!limiter.check_and_increment(ip).await);
}

#[tokio::test]
async fn different_ips_independent() {
    let limiter = IpRateLimiter::new(2);
    let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

    assert!(limiter.check_and_increment(ip1).await);
    assert!(limiter.check_and_increment(ip1).await);
    assert!(!limiter.check_and_increment(ip1).await); // ip1 exhausted

    // ip2 should still be allowed
    assert!(limiter.check_and_increment(ip2).await);
    assert!(limiter.check_and_increment(ip2).await);
    assert!(!limiter.check_and_increment(ip2).await);
}

#[tokio::test]
async fn ipv6_works() {
    let limiter = IpRateLimiter::new(1);
    let ip = IpAddr::V6(Ipv6Addr::LOCALHOST);
    assert!(limiter.check_and_increment(ip).await);
    assert!(!limiter.check_and_increment(ip).await);
}

#[tokio::test(start_paused = true)]
async fn window_resets_after_expiry() {
    let limiter = IpRateLimiter::new(1);
    let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

    assert!(limiter.check_and_increment(ip).await);
    assert!(!limiter.check_and_increment(ip).await);

    // Advance time past the 60s window
    tokio::time::advance(std::time::Duration::from_secs(61)).await;

    // Should be allowed again after window reset
    assert!(limiter.check_and_increment(ip).await);
    assert!(!limiter.check_and_increment(ip).await);
}

#[tokio::test]
async fn limit_of_one() {
    let limiter = IpRateLimiter::new(1);
    let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
    assert!(limiter.check_and_increment(ip).await);
    assert!(!limiter.check_and_increment(ip).await);
    assert!(!limiter.check_and_increment(ip).await);
}

#[tokio::test]
async fn many_ips_tracked_independently() {
    let limiter = IpRateLimiter::new(1);
    for i in 0..=255u8 {
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, i));
        assert!(
            limiter.check_and_increment(ip).await,
            "first request from 10.0.0.{i} should be allowed"
        );
    }
    // All should now be exhausted
    for i in 0..=255u8 {
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, i));
        assert!(
            !limiter.check_and_increment(ip).await,
            "second request from 10.0.0.{i} should be rejected"
        );
    }
}

#[tokio::test]
async fn ipv6_grouped_by_64() {
    let limiter = IpRateLimiter::new(1);
    let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
    let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
    let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
    assert!(limiter.check_and_increment(a).await);
    // Same /64: shares the budget
    assert!(!limiter.check_and_increment(b).await);
    // Different /64: independent
    assert!(limiter.check_and_increment(other).await);
}

#[test]
fn ipv4_mapped_ipv6_treated_as_ipv4() {
    let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
    assert_eq!(client_key(mapped), IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
}

#[test]
fn concurrency_limit_releases_on_drop() {
    let limiter = IpConcurrencyLimiter::new(2);
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let g1 = limiter.try_acquire(ip).expect("first");
    let _g2 = limiter.try_acquire(ip).expect("second");
    assert!(limiter.try_acquire(ip).is_none());
    // Other IPs are unaffected
    assert!(limiter
        .try_acquire(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)))
        .is_some());
    drop(g1);
    assert_eq!(limiter.current(ip), 1);
    assert!(limiter.try_acquire(ip).is_some());
}
