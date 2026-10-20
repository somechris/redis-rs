use redis::{ConnectionAddr, TypedCommands};
use redis_test::{TestContext, skip};

/// Tries to assure that the current server allows to parse the versions
#[test]
fn replication() {
    let mut ctx = TestContext::default();
    if !matches!(ctx.server.addr, ConnectionAddr::Tcp(_, _)) {
        skip!("replicaton is only supported for plain tcp");
    }
    let mut leader_con = ctx.connection();

    // Setting a value to begin with
    leader_con.set("foo", "bar").unwrap();

    // Start a follower and check that the value is present
    let mut follower_1_con = ctx.start_follower();
    assert_eq!(follower_1_con.get("foo").unwrap().unwrap(), "bar");

    // Update the value
    leader_con.set("foo", "baz").unwrap();

    // Start another follower and check that the updated value is present for all followers
    let mut follower_2_con = ctx.start_follower();
    assert_eq!(follower_1_con.get("foo").unwrap().unwrap(), "baz");
    assert_eq!(follower_2_con.get("foo").unwrap().unwrap(), "baz");

    // Update the value again, wait for replication to sync up, and recheck
    leader_con.set("foo", "quux").unwrap();
    ctx.wait_for_replication_sync();
    assert_eq!(follower_1_con.get("foo").unwrap().unwrap(), "quux");
    assert_eq!(follower_2_con.get("foo").unwrap().unwrap(), "quux");
}
