use agens_core::IntraTurnInputSource;
use agens_store::{DirectiveTarget, OpenQuestion, QuestionClass, QuestionStore};

fn temporary_directory(label: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "agens-questions-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&directory).ok();
    std::fs::create_dir_all(&directory).expect("the question store directory is writable");
    directory
}

#[test]
fn a_valid_external_answer_closes_the_question_for_delivery() {
    let directory = temporary_directory("valid-answer");
    let target = DirectiveTarget::Session(42);
    let mut store = QuestionStore::open(&directory).expect("the store opens");
    store
        .open_question(&OpenQuestion {
            target: target.clone(),
            question_id: "7".into(),
            class: QuestionClass::AskUser,
            origin: "ask_user".into(),
            admissible_answers: vec!["approve".into(), "decline".into()],
        })
        .expect("the question opens");

    store
        .answer(&target, "7", "approve", IntraTurnInputSource::Supervisor)
        .expect("an admissible answer is accepted");

    let answer = store
        .take_answer(&target, "7")
        .expect("the answer can be delivered")
        .expect("the answer exists");
    assert_eq!(answer.value, "approve");
    assert_eq!(answer.source, IntraTurnInputSource::Supervisor);
    assert!(
        store
            .take_answer(&target, "7")
            .expect("a delivered answer can be queried")
            .is_none(),
        "the answer is delivered exactly once"
    );

    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn a_question_can_only_be_answered_by_its_addressed_child() {
    let directory = temporary_directory("child-address");
    let addressed = DirectiveTarget::Child("child-a".into());
    let other = DirectiveTarget::Child("child-b".into());
    let mut store = QuestionStore::open(&directory).expect("the store opens");
    store
        .open_question(&OpenQuestion {
            target: addressed.clone(),
            question_id: "11".into(),
            class: QuestionClass::Consent,
            origin: "review".into(),
            admissible_answers: vec!["granted".into(), "declined".into()],
        })
        .expect("the child question opens");

    let error = store
        .answer(&other, "11", "declined", IntraTurnInputSource::Supervisor)
        .expect_err("another child cannot answer the question");

    assert!(error.to_string().contains("no open question"), "{error}");
    assert!(
        store
            .take_answer(&addressed, "11")
            .expect("the addressed question remains readable")
            .is_none()
    );

    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn an_answer_outside_the_question_domain_is_refused() {
    let directory = temporary_directory("invalid-answer");
    let target = DirectiveTarget::Child("child-ref".into());
    let mut store = QuestionStore::open(&directory).expect("the store opens");
    store
        .open_question(&OpenQuestion {
            target: target.clone(),
            question_id: "11".into(),
            class: QuestionClass::Consent,
            origin: "review".into(),
            admissible_answers: vec!["granted".into(), "declined".into()],
        })
        .expect("the child question opens");

    let error = store
        .answer(&target, "11", "maybe", IntraTurnInputSource::Supervisor)
        .expect_err("an answer outside the closed domain is refused");

    assert!(error.to_string().contains("not admissible"), "{error}");
    assert!(
        store
            .take_answer(&target, "11")
            .expect("the unanswered question remains readable")
            .is_none()
    );

    std::fs::remove_dir_all(directory).ok();
}
