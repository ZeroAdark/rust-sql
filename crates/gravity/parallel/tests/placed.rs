//! placed.rs: a statement's pool placement (`place` .. `Placed::drop`) nests — a nested statement's drop restores the outer's index.

use gravity_parallel::{install_pools, place, placed, FirstError, Job, MorselSource, Pool};

struct Idle;

impl Pool for Idle {
    fn run(&self, _width: u16, _claims: &dyn MorselSource, _job: &Job<'_>) -> Result<(), FirstError> {
        Ok(())
    }
}

static POOLS: [Idle; 2] = [Idle, Idle];

#[test]
fn a_nested_placement_restores_the_outer_index() {
    assert!(install_pools(vec![&POOLS[0], &POOLS[1]]));
    assert_eq!(placed(), 0);
    let a = place();
    assert_eq!((a.idx, placed()), (0, 0));
    let b = place();
    assert_eq!((b.idx, placed()), (1, 1), "the fewest in flight");
    {
        let c = place();
        assert_eq!((c.idx, placed()), (0, 0), "a tie goes to the lowest index");
    }
    assert_eq!(placed(), 1, "the nested placement's drop restores the outer's index, not 0");
    assert_eq!(gravity_parallel::inflight(), vec![1, 1]);
    drop(b);
    assert_eq!(placed(), 0);
    drop(a);
    assert_eq!(placed(), 0);
    assert_eq!(gravity_parallel::inflight(), vec![0, 0]);
}
