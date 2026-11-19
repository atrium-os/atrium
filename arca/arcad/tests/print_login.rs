use arca_store::codec::Writer;
use arcad::{
    label::Label,
    proto::{op, status},
    server::State,
};

const UUID: &str = "0bc4ab0f-6451-4bd5-a146-ca6ce118ba21";
const KEY: [u8; 32] = [23; 32];

fn binding(persona: &str) -> Writer {
    printer_binding(persona, &KEY)
}

fn printer_binding(persona: &str, key: &[u8; 32]) -> Writer {
    let mut w = Writer::new();
    w.str("tester");
    w.str(persona);
    w.raw(key);
    w.str(UUID);
    w
}

#[test]
fn printer_login_persists_and_only_the_bound_courier_can_redeem_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Writer::new();
    root.str("tester");
    root.raw(&[10; 32]);
    {
        let mut state = State::new(dir.path().into()).unwrap();
        for opcode in [op::V_CREATE, op::V_UNLOCK] {
            assert_eq!(
                state.handle(&Label::Vestibulum, opcode, &root.buf).status,
                status::OK
            );
        }
        for password in [b"first".as_slice(), b"replacement"] {
            let mut entry = binding("work");
            entry.str("printer-account");
            entry.bytes(password);
            assert_eq!(
                state
                    .handle(&Label::TrustedUi, op::U_STORE_PRINT_LOGIN, &entry.buf)
                    .status,
                status::OK
            );
        }
    }

    let mut state = State::new(dir.path().into()).unwrap();
    assert_eq!(
        state
            .handle(&Label::Vestibulum, op::V_UNLOCK, &root.buf)
            .status,
        status::OK
    );
    let grant = |state: &mut State, persona| {
        let reply = state.handle(
            &Label::PrintBroker,
            op::PRINT_LOGIN_GRANT,
            &binding(persona).buf,
        );
        assert_eq!(reply.status, status::OK);
        reply.body.bytes().to_vec()
    };
    assert!(grant(&mut state, "personal").is_empty());
    let token = grant(&mut state, "work");
    assert_eq!(token.len(), 16);
    let take = |key: [u8; 32], token: &[u8]| {
        let mut w = Writer::new();
        w.raw(token);
        w.raw(&key);
        w.str(UUID);
        w
    };
    assert_eq!(
        state
            .handle(
                &Label::PrintWorker,
                op::TAKE_PRINT_LOGIN,
                &take([0; 32], &token).buf
            )
            .status,
        status::DENIED
    );
    assert_eq!(
        state
            .handle(
                &Label::PrintWorker,
                op::TAKE_PRINT_LOGIN,
                &take(KEY, &token).buf
            )
            .status,
        status::DENIED
    );
    let request = take(KEY, &grant(&mut state, "work"));
    assert_eq!(
        state
            .handle(&Label::PrintBroker, op::TAKE_PRINT_LOGIN, &request.buf)
            .status,
        status::DENIED
    );
    let reply = state.handle(&Label::PrintWorker, op::TAKE_PRINT_LOGIN, &request.buf);
    assert_eq!(reply.status, status::OK);
    assert!(reply.body.bytes() == b"printer-account:replacement");
    assert_eq!(
        state
            .handle(&Label::PrintWorker, op::TAKE_PRINT_LOGIN, &request.buf)
            .status,
        status::DENIED
    );
}

#[test]
fn saved_pin_is_separate_from_login_and_redeemable_once_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Writer::new();
    root.str("tester");
    root.raw(&[10; 32]);
    {
        let mut state = State::new(dir.path().into()).unwrap();
        for opcode in [op::V_CREATE, op::V_UNLOCK] {
            assert_eq!(state.handle(&Label::Vestibulum, opcode, &root.buf).status, status::OK);
        }
        let mut login = binding("work");
        login.str("account");
        login.bytes(b"password");
        assert_eq!(state.handle(&Label::TrustedUi, op::U_STORE_PRINT_LOGIN, &login.buf).status, status::OK);
        for pin in [b"0123", b"0427"] {
            let mut save = binding("work");
            save.bytes(pin);
            let r = state.handle(&Label::TrustedUi, op::U_SAVE_PRINT_PIN, &save.buf);
            assert_eq!(r.status, status::OK);
            assert_eq!(r.body.bytes().len(), 16);
        }
    }
    let mut state = State::new(dir.path().into()).unwrap();
    assert_eq!(state.handle(&Label::Vestibulum, op::V_UNLOCK, &root.buf).status, status::OK);
    assert_eq!(state.handle(&Label::TrustedUi, op::U_USE_SAVED_PRINT_PIN, &binding("personal").buf).status, status::DENIED);
    let other_printer = printer_binding("work", &[1; 32]);
    assert_eq!(state.handle(&Label::TrustedUi, op::U_USE_SAVED_PRINT_PIN, &other_printer.buf).status, status::DENIED);
    assert_ne!(state.handle(&Label::PrintBroker, op::U_USE_SAVED_PRINT_PIN, &binding("work").buf).status, status::OK);
    for _ in 0..2 {
        let grant = state.handle(&Label::TrustedUi, op::U_USE_SAVED_PRINT_PIN, &binding("work").buf);
        assert_eq!(grant.status, status::OK);
        let mut take = Writer::new();
        take.raw(grant.body.bytes());
        take.raw(&binding("work").buf);
        assert_ne!(state.handle(&Label::PrintBroker, op::TAKE_PRINT_PIN, &take.buf).status, status::OK);
        let pin = state.handle(&Label::PrintWorker, op::TAKE_PRINT_PIN, &take.buf);
        assert_eq!(pin.status, status::OK);
        assert!(pin.body.bytes() == b"0427");
        assert_eq!(state.handle(&Label::PrintWorker, op::TAKE_PRINT_PIN, &take.buf).status, status::DENIED);
    }
    let login = state.handle(&Label::PrintBroker, op::PRINT_LOGIN_GRANT, &binding("work").buf);
    let mut take = Writer::new();
    take.raw(login.body.bytes());
    take.raw(&KEY);
    take.str(UUID);
    assert!(state.handle(&Label::PrintWorker, op::TAKE_PRINT_LOGIN, &take.buf).body.bytes() == b"account:password");
}
