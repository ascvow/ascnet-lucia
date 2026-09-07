use sample_repo::config::settings::Settings;
use sample_repo::RetryPolicy;

#[test]
fn retry_gives_up_after_max_attempts() {
    let policy = RetryPolicy::from_settings(&Settings::default());
    let mut seen = Vec::new();
    let result = policy.execute(|attempt| {
        seen.push(attempt);
        Err::<(), String>("boom".to_string())
    });
    assert!(result.is_err());
    assert_eq!(seen.len(), 3);
}

#[test]
fn retry_succeeds_on_second_attempt() {
    let policy = RetryPolicy::from_settings(&Settings::default());
    let result = policy.execute(|attempt| {
        if attempt < 2 {
            Err("temporary".to_string())
        } else {
            Ok(attempt)
        }
    });
    assert_eq!(result.expect("should succeed"), 2);
}
