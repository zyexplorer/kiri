//! One call's conditional rules as a program: libseccomp's rule tree for the
//! call (`tree.rs`), written out as tests.
//!
//! Each node of the tree is one test of a 32-bit half of an argument, as in
//! libseccomp's filter generator (`_gen_bpf_chain`): a node that decides goes
//! to its action or to the level below, anything else goes to the next node
//! of its level, and a level that fails goes to the next node of the level
//! above. Nodes with the same next node share it, so the code is linear in
//! the tree.
//!
//! Two things are added to the tree's code so that an answer is the action of
//! a rule that holds, or the default when none does, whatever shape the tree
//! has: where an action stands for a rule of more than one condition (the
//! tree's nodes there may be another rule's) the action goes to the rules
//! that have it, in profile order, and the call to the rules one after the
//! other if none does; and after the tree come the rules of more than one
//! condition and the rules the tree lost, in profile order. A single
//! condition's nodes can only be that rule's own, so the action stands as it
//! is. A tree that loops (libseccomp accepts them at create, and runc hangs
//! at start) or is too large to follow is read rule by rule, in the order the
//! tree reaches the rules' actions.
//!
//! Every jump goes forward, so the program cannot loop.

use std::collections::{HashMap, HashSet};

use super::tree::{Branch, Db, Leaf, Who};
use super::{Cond, JEQ_K, JGE_K, JGT_K, OFF_ARGS, Op};

/// Where a test sends the filter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tgt {
    /// No rule matched: on to the call's default.
    End,
    Act(u32),
    /// Another test of the same call, by position.
    Inst(usize),
}

/// One test: load the half-argument at `off`, mask it unless the mask is all
/// ones, compare with `datum`.
pub struct TInst {
    pub off: u32,
    pub mask: u32,
    pub code: u16,
    pub datum: u32,
    pub t: Tgt,
    pub f: Tgt,
}

/// (high half, low half) of what a condition compares with: the value, or for
/// MASKED_EQ the second value under the mask.
fn halves(c: &Cond) -> (u32, u32) {
    let d = if c.op == Op::MaskedEq { c.datum & c.value } else { c.value };
    ((d >> 32) as u32, d as u32)
}

/// One step of a rule: one test of one half of an argument (`neg` for the
/// opposite of what the instruction says), or a whole range condition.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Cmp { off: u32, mask: u32, code: u16, datum: u32, neg: bool },
    Range { index: u32, op: Op, value: u64 },
}

fn steps(conds: &[Cond]) -> Vec<Step> {
    let mut out = Vec::new();
    for c in conds {
        let base = OFF_ARGS + 8 * c.index;
        match c.op {
            Op::Eq | Op::MaskedEq => {
                let mask = if c.op == Op::MaskedEq { c.value } else { u64::MAX };
                let (dh, dl) = halves(c);
                out.push(Step::Cmp { off: base + 4, mask: (mask >> 32) as u32, code: JEQ_K, datum: dh, neg: false });
                out.push(Step::Cmp { off: base, mask: mask as u32, code: JEQ_K, datum: dl, neg: false });
            }
            op => out.push(Step::Range { index: c.index, op, value: c.value }),
        }
    }
    out
}

/// The steps for the part of a rule's last condition that one of libseccomp's
/// tree nodes stands for: the high half alone, or the low half once the high
/// halves are equal.
fn leaf_steps(c: &Cond, leaf: Leaf) -> Vec<Step> {
    let (hi, lo) = (OFF_ARGS + 8 * c.index + 4, OFF_ARGS + 8 * c.index);
    let (h, l) = ((c.value >> 32) as u32, c.value as u32);
    let cmp = |off, code, datum, neg| Step::Cmp { off, mask: u32::MAX, code, datum, neg };
    match (c.op, leaf) {
        (Op::Gt | Op::Ge, Leaf::Hi) => vec![cmp(hi, JGT_K, h, false)],
        (Op::Gt, Leaf::Lo) => vec![cmp(hi, JEQ_K, h, false), cmp(lo, JGT_K, l, false)],
        (Op::Ge, Leaf::Lo) => vec![cmp(hi, JEQ_K, h, false), cmp(lo, JGE_K, l, false)],
        (Op::Lt | Op::Le, Leaf::Hi) => vec![cmp(hi, JGE_K, h, true)],
        (Op::Lt, Leaf::Lo) => vec![cmp(hi, JEQ_K, h, false), cmp(lo, JGE_K, l, true)],
        (Op::Le, Leaf::Lo) => vec![cmp(hi, JEQ_K, h, false), cmp(lo, JGT_K, l, true)],
        (Op::Ne, Leaf::Hi) => vec![cmp(hi, JEQ_K, h, true)],
        (Op::Ne, Leaf::Lo) => vec![cmp(hi, JEQ_K, h, false), cmp(lo, JEQ_K, l, true)],
        _ => steps(std::slice::from_ref(c)),
    }
}

struct Builder {
    out: Vec<TInst>,
    /// Tests still allowed in the whole filter.
    budget: usize,
    over: bool,
}

impl Builder {
    fn push(&mut self, off: u32, mask: u32, code: u16, datum: u32, t: Tgt, f: Tgt) -> Tgt {
        if self.budget == 0 {
            self.over = true;
            return Tgt::End;
        }
        self.budget -= 1;
        self.out.push(TInst { off, mask, code, datum, t, f });
        Tgt::Inst(self.out.len() - 1)
    }

    /// The entry of a test that goes to `m` when the step holds, else `f`.
    fn step(&mut self, s: Step, m: Tgt, f: Tgt) -> Tgt {
        match s {
            Step::Cmp { off, mask, code, datum, neg } => {
                if neg {
                    self.push(off, mask, code, datum, f, m)
                } else {
                    self.push(off, mask, code, datum, m, f)
                }
            }
            Step::Range { index, op, value } => {
                let (hi, lo) = (OFF_ARGS + 8 * index + 4, OFF_ARGS + 8 * index);
                let (h, l) = ((value >> 32) as u32, value as u32);
                match op {
                    Op::Gt | Op::Ge => {
                        let t3 = self.push(lo, u32::MAX, if op == Op::Gt { JGT_K } else { JGE_K }, l, m, f);
                        let t2 = self.push(hi, u32::MAX, JEQ_K, h, t3, f);
                        self.push(hi, u32::MAX, JGT_K, h, m, t2)
                    }
                    Op::Lt | Op::Le => {
                        let t3 = self.push(lo, u32::MAX, if op == Op::Lt { JGE_K } else { JGT_K }, l, f, m);
                        let t2 = self.push(hi, u32::MAX, JEQ_K, h, t3, m);
                        self.push(hi, u32::MAX, JGT_K, h, f, t2)
                    }
                    Op::Ne => {
                        let t2 = self.push(lo, u32::MAX, JEQ_K, l, f, m);
                        self.push(hi, u32::MAX, JEQ_K, h, t2, m)
                    }
                    // Equality is split into half tests before it gets here; if
                    // it did not, the call is read as too large.
                    Op::Eq | Op::MaskedEq => {
                        self.over = true;
                        Tgt::End
                    }
                }
            }
        }
    }

    /// The rules in order, from step `d` on, each already known to share its
    /// first `d` steps with the others; `fail` once none holds. Adjacent rules
    /// with the same step share its test.
    fn list(&mut self, rules: &[(Vec<Step>, u32)], d: usize, fail: Tgt) -> Tgt {
        // A rule with no step left holds: nothing after it is reached.
        let (rules, mut fail) = match rules.iter().position(|r| r.0.len() == d) {
            Some(i) => (&rules[..i], Tgt::Act(rules[i].1)),
            None => (rules, fail),
        };
        let mut runs = Vec::new();
        let mut s = 0;
        while s < rules.len() {
            let mut e = s + 1;
            while e < rules.len() && rules[e].0[d] == rules[s].0[d] {
                e += 1;
            }
            runs.push((s, e));
            s = e;
        }
        for &(s, e) in runs.iter().rev() {
            let inner = self.list(&rules[s..e], d + 1, fail);
            fail = self.step(rules[s].0[d], inner, fail);
        }
        fail
    }
}

// Tests only: compile libseccomp's tree with nothing added to it, to compare
// the tree with what libseccomp's filter generator makes of it.
#[cfg(test)]
thread_local! {
    pub static RAW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Tests only: make the tree's code hold a backward jump, and count the calls
// read rule by rule.
#[cfg(test)]
thread_local! {
    pub static BACK_EDGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub static BY_RULE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Why a call's rules do not compile.
pub enum Fail {
    /// libseccomp refuses these rules (EEXIST), or its tree loops on them.
    Conflict,
    /// More tests than the filter may hold.
    TooLarge,
}

/// The tests for one call's conditional rules (each non-empty, its conditions
/// in argument order, in profile order), with every jump going forward and
/// the first test first. `budget` is how many tests the whole filter may
/// still hold.
pub fn compile(rules: &[(Vec<Cond>, u32)], budget: &mut usize, pool: &mut u64) -> Result<Vec<TInst>, Fail> {
    // A rule that comes twice stands once, as in libseccomp.
    let mut seen = HashSet::new();
    let once: Vec<(Vec<Cond>, u32)> = rules.iter().filter(|(c, a)| seen.insert((key(c), *a))).cloned().collect();
    let rules = &once[..];
    // Every rule that stays needs a test of its own, or is the longer of two
    // rules and is dropped: a call with more rules than tests is no loss.
    if rules.len() > *budget {
        return Err(Fail::TooLarge);
    }
    let mut db = Db::with_pool(*pool);
    for (i, (conds, action)) in rules.iter().enumerate() {
        db.add(i, *action, conds).map_err(|()| Fail::Conflict)?;
    }
    #[cfg(test)]
    if RAW.with(|r| r.get()) {
        let mut em = Em::new(&db, rules, 100_000, false);
        em.raw = true;
        return em.run(&[]).map(|(v, _)| v).map_err(|_| Fail::TooLarge);
    }
    // The profile's steps for walking trees are gone: no more tree work, the
    // rules are read one after the other, which is sound.
    if db.pool() == 0 {
        let mut left = *budget;
        let out = slots(rules, Vec::new(), &HashSet::new(), &mut left);
        if out.is_ok() {
            *budget = left;
        }
        return out;
    }
    let placed = db.order();
    *pool = db.pool();
    let have: HashSet<Who> = placed.iter().copied().collect();
    // Rules that are not in the tree as they should be get another chance
    // after it: a rule with more than one condition, whose tree can have lost
    // one way into it, and any rule whose action the tree dropped.
    let tail: Vec<usize> = (0..rules.len()).filter(|&r| rules[r].0.len() > 1 || !needed(&rules[r].0).iter().all(|l| have.contains(&(r, *l)))).collect();
    let mut left = *budget;
    // Where the tree gets to an action of a rule that does not hold, the
    // action goes to a rule that has it and holds, or the rest of the rules
    // are read; if that is too much code, to the rule's own conditions.
    let out = match Em::new(&db, rules, left, true).run(&tail).or_else(|_| Em::new(&db, rules, left, false).run(&tail)) {
        Ok((v, rest)) => {
            left = rest;
            Ok(v)
        }
        // A tree that loops, or is too large to follow, is read rule by rule.
        Err(_) => slots(rules, placed, &have, &mut left),
    };
    if out.is_ok() {
        *budget = left;
    }
    out
}

/// What a rule's conditions are: the same set is the same key.
fn key(conds: &[Cond]) -> Vec<(u32, u8, u64, u64)> {
    conds
        .iter()
        .map(|c| match c.op {
            Op::MaskedEq => (c.index, 6, c.value, c.datum & c.value),
            op => (c.index, op as u8, c.value, 0),
        })
        .collect()
}

/// The parts of a rule's last condition its action stands at in libseccomp's
/// tree.
fn needed(conds: &[Cond]) -> &'static [Leaf] {
    match conds.last().map(|c| c.op) {
        None => &[],
        Some(Op::Eq | Op::MaskedEq) => &[Leaf::Whole],
        Some(_) => &[Leaf::Hi, Leaf::Lo],
    }
}

/// The rules one after the other, in the order libseccomp's tree reaches
/// their actions, then what the tree left out: the same answers as the tree
/// where it is right, and a rule that holds in any case.
fn slots(rules: &[(Vec<Cond>, u32)], mut placed: Vec<Who>, have: &HashSet<Who>, budget: &mut usize) -> Result<Vec<TInst>, Fail> {
    for (r, (conds, _)) in rules.iter().enumerate() {
        placed.extend(needed(conds).iter().map(|l| (r, *l)).filter(|w| !have.contains(w)));
    }
    let stepped: Vec<(Vec<Step>, u32)> = placed
        .into_iter()
        .map(|(r, leaf)| {
            let (conds, action) = &rules[r];
            // A rule has a condition (`needed` is empty for one with none), so
            // `split_last` always splits.
            let (before, last) = conds.split_at(conds.len().saturating_sub(1));
            let mut s = steps(before);
            for c in last {
                s.extend(leaf_steps(c, leaf));
            }
            (s, *action)
        })
        .collect();
    #[cfg(test)]
    BY_RULE.with(|c| c.set(c.get() + 1));
    let mut b = Builder { out: Vec::new(), budget: *budget, over: false };
    let entry = b.list(&stepped, 0, Tgt::End);
    *budget = b.budget;
    if b.over {
        return Err(Fail::TooLarge);
    }
    let Tgt::Inst(first) = entry else { return Err(Fail::TooLarge) };
    forward(b.out, first).map_err(|_| Fail::TooLarge)
}

/// libseccomp's tree as code: a test per node, as `_gen_bpf_chain` makes
/// them, with the one change that where an action stands for a rule of more
/// than one condition that rule's conditions are tested first, so that the
/// tree cannot answer with the action of a rule that does not hold.
struct Em<'a> {
    db: &'a Db,
    rules: &'a [(Vec<Cond>, u32)],
    b: Builder,
    memo: HashMap<(usize, Tgt), Tgt>,
    guards: HashMap<(usize, Tgt), Tgt>,
    depth: usize,
    /// Tests only: no rule's conditions are tested at an action.
    raw: bool,
    /// An action of a rule that does not hold goes to the rules that have it.
    exact: bool,
    /// Every rule, one after the other, once something needs them.
    all: Option<Tgt>,
    alts: HashMap<u32, Tgt>,
}

struct Unfit;

/// How deep levels may nest: libseccomp's own trees have a few, one for
/// each condition of a rule.
const MAX_DEPTH: usize = 64;

impl<'a> Em<'a> {
    fn new(db: &'a Db, rules: &'a [(Vec<Cond>, u32)], budget: usize, exact: bool) -> Em<'a> {
        Em { db, rules, b: Builder { out: Vec::new(), budget, over: false }, memo: HashMap::new(), guards: HashMap::new(), depth: 0, raw: false, exact, all: None, alts: HashMap::new() }
    }

    /// The code and the tests left in the budget.
    fn run(mut self, tail: &[usize]) -> Result<(Vec<TInst>, usize), Unfit> {
        let stepped: Vec<(Vec<Step>, u32)> = tail.iter().map(|&r| (steps(&self.rules[r].0), self.rules[r].1)).collect();
        let after = self.b.list(&stepped, 0, Tgt::End);
        let entry = match self.db.first_level() {
            Some(head) => self.level(head, after)?,
            None => after,
        };
        if self.b.over {
            return Err(Unfit);
        }
        let Tgt::Inst(first) = entry else { return Err(Unfit) };
        let left = self.b.budget;
        #[cfg(test)]
        if BACK_EDGE.with(|b| b.get()) {
            let n = self.b.out.len();
            self.b.out[n - 1].t = Tgt::Inst(0);
            self.b.out[0].f = Tgt::Inst(n - 1);
        }
        Ok((forward(self.b.out, first)?, left))
    }

    /// The first test of a level that falls through to `fall`.
    fn level(&mut self, head: usize, fall: Tgt) -> Result<Tgt, Unfit> {
        if self.depth >= MAX_DEPTH {
            return Err(Unfit);
        }
        self.depth += 1;
        let mut nodes = Vec::new();
        let mut c = Some(head);
        while let Some(i) = c {
            if nodes.len() > self.b.budget {
                return Err(Unfit);
            }
            nodes.push(i);
            c = self.db.sibling(i);
        }
        let mut next = fall;
        for &i in nodes.iter().rev() {
            next = self.node(i, fall, next)?;
        }
        self.depth -= 1;
        Ok(next)
    }

    fn node(&mut self, i: usize, fall: Tgt, next: Tgt) -> Result<Tgt, Unfit> {
        if let Some(t) = self.memo.get(&(i, fall)) {
            return Ok(*t);
        }
        let t = self.db.test(i);
        let Tgt::Inst(at) = self.b.push(t.off, t.mask, t.code, t.datum, Tgt::End, Tgt::End) else { return Err(Unfit) };
        self.memo.insert((i, fall), Tgt::Inst(at));
        let tt = self.branch(i, true, next)?;
        let ff = self.branch(i, false, next)?;
        self.b.out[at].t = tt;
        self.b.out[at].f = ff;
        Ok(Tgt::Inst(at))
    }

    /// The first rule with this action whose conditions hold; failing that,
    /// the first rule of all that holds.
    fn alt(&mut self, action: u32) -> Tgt {
        if let Some(t) = self.alts.get(&action) {
            return *t;
        }
        let all = match self.all {
            Some(a) => a,
            None => {
                let stepped: Vec<(Vec<Step>, u32)> = self.rules.iter().map(|(c, a)| (steps(c), *a)).collect();
                let a = self.b.list(&stepped, 0, Tgt::End);
                self.all = Some(a);
                a
            }
        };
        let own: Vec<(Vec<Step>, u32)> = self.rules.iter().filter(|(_, a)| *a == action).map(|(c, a)| (steps(c), *a)).collect();
        let t = self.b.list(&own, 0, all);
        self.alts.insert(action, t);
        t
    }

    fn branch(&mut self, i: usize, t: bool, next: Tgt) -> Result<Tgt, Unfit> {
        match self.db.branch(i, t) {
            Branch::Level(h) => self.level(h, next),
            Branch::Nothing => Ok(next),
            Branch::Act(a, (r, _)) => {
                let Some((conds, action)) = self.rules.get(r) else { return Ok(next) };
                if self.raw || conds.len() == 1 && *action == a {
                    // The nodes on the way are this rule's own.
                    return Ok(Tgt::Act(a));
                }
                if self.exact {
                    return Ok(self.alt(a));
                }
                if let Some(g) = self.guards.get(&(r, next)) {
                    return Ok(*g);
                }
                let mut m = Tgt::Act(*action);
                for s in steps(conds).into_iter().rev() {
                    m = self.b.step(s, m, next);
                }
                if self.b.over {
                    return Err(Unfit);
                }
                self.guards.insert((r, next), m);
                Ok(m)
            }
        }
    }
}

/// The tests reachable from `first`, in reverse post-order, so that every jump
/// goes forward and `first` is first.
///
/// A jump that would go backward (a cycle: a test reached again from its own
/// branches) is `Unfit`, so the call is read rule by rule instead.
fn forward(out: Vec<TInst>, first: usize) -> Result<Vec<TInst>, Unfit> {
    let mut seen = vec![false; out.len()];
    let mut done = vec![false; out.len()];
    let mut post = Vec::with_capacity(out.len());
    let mut stack = vec![(first, 0u8)];
    seen[first] = true;
    while let Some((i, st)) = stack.pop() {
        if st < 2 {
            stack.push((i, st + 1));
            if let Tgt::Inst(j) = if st == 0 { out[i].t } else { out[i].f } {
                if j >= out.len() {
                    return Err(Unfit);
                }
                if seen[j] && !done[j] {
                    // Still being walked: this edge closes a cycle.
                    return Err(Unfit);
                }
                if !std::mem::replace(&mut seen[j], true) {
                    stack.push((j, 0));
                }
            }
        } else {
            done[i] = true;
            post.push(i);
        }
    }
    post.reverse();
    let mut at = vec![usize::MAX; out.len()];
    for (new, old) in post.iter().enumerate() {
        at[*old] = new;
    }
    let fix = |t: Tgt| if let Tgt::Inst(j) = t { Tgt::Inst(at[j]) } else { t };
    Ok(post.iter().map(|&i| TInst { off: out[i].off, mask: out[i].mask, code: out[i].code, datum: out[i].datum, t: fix(out[i].t), f: fix(out[i].f) }).collect())
}
