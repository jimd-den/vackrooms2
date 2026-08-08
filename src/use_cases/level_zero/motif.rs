//! The world's signature, and the ways it is broken.
//!
//! Correct architecture is not frightening. A building that is merely
//! plausible everywhere is a set; what unsettles someone is a place that
//! establishes a pattern, teaches it until it is expected, and then departs
//! from it in one specific respect. That is the finding behind Diel & Lewis
//! (2022) — structural *deviations* drive the uncanny valley of physical
//! places — and it is only available to a generator that has a pattern to
//! deviate from in the first place.
//!
//! So each seed draws one [`RoomMotif`]: this world's habitual room. Most
//! rooms are it, give or take. That repetition is not monotony, it is the
//! setup — the "one big yellow blur" the canon describes, and the reason
//! anything else lands at all.
//!
//! Three departures from it, in rising order of wrongness:
//!
//! * [`Deviation::Variation`] — the same room, slightly off. Not wrong
//!   enough to point at.
//! * [`Deviation::Break`] — the motif contradicted in exactly *one*
//!   respect. One is the whole trick: contradict two and it reads as a
//!   different room, which is unremarkable. Contradict one and it reads as
//!   *this* room, wrong.
//! * [`Deviation::Exaggeration`] — the motif on steroids. Same grammar,
//!   pushed until it stops being habitable.
//!
//! ## Familiarity: the level answers back
//!
//! `familiarity` is how long someone has settled in an area — supplied by
//! the engine, which is the only thing that can know. It does not make the
//! world more hostile; it makes it more *attentive*. As familiarity rises,
//! variations and breaks concentrate: the place starts offering back the
//! room you got comfortable in, not quite right.
//!
//! The Peripheral Shift changes what you are not looking at. This changes
//! what you *are*.

use crate::use_cases::ports::NoiseProvider;

use super::BackroomsLevel;

/// The character of a world's habitual room.
///
/// Not a knob but a *kind of place* — each of these is a different answer
/// to "what is this building like", and a seed commits to one. They are
/// deliberately few and strongly distinguishable: a vocabulary you can
/// learn in ten minutes of walking is one you can then be lied to with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Signature {
    /// Empty rooms ringed with far too many doors. Nothing to look at and
    /// nowhere obviously to go: the room is all thresholds and no content,
    /// which turns every arrival into a decision you have no basis for.
    DoorGlut,
    /// Interesting minimalism. One opening, one fixture, nothing else, and
    /// the proportions good enough that the emptiness reads as *intended*
    /// rather than unfinished — which is far worse.
    Minimal,
    /// Repeating columns, endless rhythm, no destination. The bay spacing
    /// becomes the only way to measure distance, and it never changes.
    Colonnade,
    /// A warren of small cells. Every room is a room too small to be for
    /// anything.
    Cellular,
    /// Ceilings pressed lower than the span asks for.
    Pressed,
    /// Ceilings lifted higher than the span asks for.
    Lofty,
}

impl Signature {
    fn of(index: u32) -> Self {
        match index % 6 {
            0 => Signature::DoorGlut,
            1 => Signature::Minimal,
            2 => Signature::Colonnade,
            3 => Signature::Cellular,
            4 => Signature::Pressed,
            _ => Signature::Lofty,
        }
    }
}

/// A world's habitual room: what "normal" means for this seed.
///
/// A bundle rather than a scalar, because a place is a set of decisions that
/// agree with each other. A door-glut room is empty *and* over-connected;
/// a minimal room is bare *and* under-connected. Reading those from separate
/// fields would let the generator produce a room that is both, which is not
/// a character, it is noise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RoomMotif {
    pub signature: Signature,
    /// Ceiling offset the signature applies, world units.
    pub ceiling_bias: f32,
    /// Multiplier on how readily walls are pierced.
    pub door_density: f32,
    /// Multiplier on how much furniture belongs here.
    pub furnish_density: f32,
    /// The subdivision level this world's rooms prefer.
    pub preferred_level: u32,
    /// Colossal solid masses stand in this space.
    pub monumental: bool,
}

/// The motif for a world. One draw per seed, and every room answers to it.
pub(super) fn motif_for(seed: u32) -> RoomMotif {
    // Hashed off the seed alone — not off position, and not off noise.
    // A world has one signature everywhere, or it is not a signature.
    let mut h = seed ^ 0x5EED_0417;
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    let signature = Signature::of(h);
    let (ceiling_bias, door_density, furnish_density, preferred_level) = match signature {
        // Over-connected and stripped: all the ways out, nothing to stay for.
        Signature::DoorGlut => (0.0, 2.6, 0.15, 1),
        // Under-connected and bare, but generously proportioned.
        Signature::Minimal => (0.4, 0.35, 0.1, 2),
        Signature::Colonnade => (0.6, 0.8, 0.3, 3),
        Signature::Cellular => (-0.4, 1.4, 0.6, 0),
        Signature::Pressed => (-0.6, 1.0, 0.8, 1),
        Signature::Lofty => (0.8, 0.9, 0.5, 2),
    };
    RoomMotif {
        signature,
        ceiling_bias,
        door_density,
        furnish_density,
        preferred_level,
        monumental: false,
    }
}

/// Brutalism, breaking in.
///
/// Brutalism is not a material, it is a *theory of mass*: magnificent scale,
/// monolithic blocks, form asserted without ornament or apology. So this
/// changes no surface — Level 0 stays yellow wallpaper throughout, and that
/// is exactly what makes it land. A monstrous monumental mass wearing the
/// office's own wallpaper is far worse than an honest concrete one, because
/// the building has not changed material, it has changed *intention*.
///
/// It arrives in blobs with hard edges: you walk out of an ordinary papered
/// warren and into something built at a scale nothing here needs, and then
/// back out again.
///
/// It gets its own field rather than a slot in the signature list because it
/// contradicts the motif rather than varying it. Every other departure here
/// is the world disagreeing with itself by degrees; this is a different
/// architecture pushing through. A world whose *habit* was monumental would
/// simply be a monumental world, and nothing would be breaking in.
pub(super) fn brutalist_incursion(noise: &dyn NoiseProvider, seed: u32, wx: f32, wz: f32) -> f32 {
    // Smooth field, hard threshold: blobs with edges you cross, not a
    // gradient you drift through. An incursion you cannot point at the edge
    // of is a mood, and this is supposed to be a thing.
    let field = BackroomsLevel::n(noise, seed, 0xB247, wx, wz, 0.045);
    let over = field - 0.62;
    if over <= 0.0 {
        0.0
    } else {
        (over / 0.18).clamp(0.0, 1.0)
    }
}

/// Does a colossal mass stand at this point?
///
/// Inside an incursion, whole blocks of the plan are simply *solid* — not
/// rooms with thick walls, but masses with no interior at all, on a lattice
/// far coarser than anything else in the level. You do not enter them; you
/// walk around them, and the walk is long.
///
/// That is the brutalist move and the reason it needs its own rule: every
/// other wall in Level 0 encloses something. These enclose nothing, which is
/// a kind of object the fabric had no way to express — its whole vocabulary
/// was rooms and the boundaries between them.
pub(super) fn monolith_at(noise: &dyn NoiseProvider, seed: u32, wx: f32, wz: f32) -> bool {
    let strength = brutalist_incursion(noise, seed, wx, wz);
    if strength <= 0.0 {
        return false;
    }
    // Masses on a 28.8 u lattice: big enough that one fills the view and you
    // must commit to a direction to get past it.
    const MASS: f32 = 28.8;
    let (bx, bz) = ((wx / MASS).floor() as i64, (wz / MASS).floor() as i64);
    if BackroomsLevel::cell_hash(noise, seed, 0xB105, bx, bz) > 0.34 * strength + 0.10 {
        return false;
    }
    // A solid core inside the block, leaving a skirt of floor around it so
    // the mass reads as an object standing in space rather than as the space
    // having been walled off.
    let (fx, fz) = (wx.rem_euclid(MASS), wz.rem_euclid(MASS));
    let margin = MASS * 0.22;
    fx > margin && fx < MASS - margin && fz > margin && fz < MASS - margin
}

/// The motif in force at a point, after any incursion.
pub(super) fn motif_at(noise: &dyn NoiseProvider, seed: u32, wx: f32, wz: f32) -> RoomMotif {
    let mut motif = motif_for(seed);
    if brutalist_incursion(noise, seed, wx, wz) > 0.0 {
        // The intruding grammar overrides the world's, wholesale. Half a
        // conversion would read as a renovation; this is not a renovation.
        // Scale asserted, ornament refused, and the space no longer for
        // anything. Nothing about the surface changes.
        motif.monumental = true;
        motif.ceiling_bias += 1.8;
        motif.furnish_density = 0.0;
        motif.door_density *= 0.3;
    }
    motif
}

/// How a particular room departs from the world's motif.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Deviation {
    /// The habitual room. Most of the world.
    Habitual,
    /// The same room, slightly off.
    Variation,
    /// The motif contradicted in exactly one respect.
    Break,
    /// The motif pushed past habitability.
    Exaggeration,
}

impl Deviation {
    /// Multiplier applied to whatever the signature does to a room.
    ///
    /// A break *inverts* rather than merely changing: the room where the
    /// ceiling goes the wrong way is the one you notice, because it is the
    /// only fact about the room that disagrees with every other room.
    pub fn intensity(self) -> f32 {
        match self {
            Deviation::Habitual => 1.0,
            Deviation::Variation => 1.35,
            Deviation::Break => -1.0,
            Deviation::Exaggeration => 3.0,
        }
    }
}

/// Which departure this room makes.
///
/// `familiarity` in 0..=1 is how settled someone is here. At 0 the world is
/// almost entirely habitual and the rare break is a genuine event. As it
/// rises, variation becomes common and breaks close in — the level noticing
/// that you had stopped noticing it.
pub(super) fn deviation_at(
    noise: &dyn NoiseProvider,
    seed: u32,
    cx: i64,
    cz: i64,
    familiarity: f32,
) -> Deviation {
    let f = familiarity.clamp(0.0, 1.0);
    let roll = BackroomsLevel::cell_hash(noise, seed, 0x0DE0_0001, cx, cz);
    // Exaggeration stays rare at any familiarity: it is the level shouting,
    // and a thing that shouts constantly is not frightening, it is loud.
    let exaggerated = 0.015 + 0.02 * f;
    let broken = exaggerated + 0.05 + 0.12 * f;
    let varied = broken + 0.10 + 0.35 * f;
    if roll < exaggerated {
        Deviation::Exaggeration
    } else if roll < broken {
        Deviation::Break
    } else if roll < varied {
        Deviation::Variation
    } else {
        Deviation::Habitual
    }
}

/// The ceiling offset this room carries: the motif's signature, scaled by
/// how this particular room departs from it.
pub(super) fn ceiling_offset(
    noise: &dyn NoiseProvider,
    seed: u32,
    cx: i64,
    cz: i64,
    wx: f32,
    wz: f32,
    familiarity: f32,
) -> f32 {
    let motif = motif_at(noise, seed, wx, wz);
    let deviation = deviation_at(noise, seed, cx, cz, familiarity);
    motif.ceiling_bias * deviation.intensity()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::position::Position;

    struct TestNoise;
    impl NoiseProvider for TestNoise {
        fn evaluate_2d(&self, seed: u32, position: Position) -> f32 {
            let mut h = seed
                .wrapping_add((position.x as i32) as u32 ^ 0x9E37_79B9)
                .wrapping_add((position.z as i32) as u32 ^ 0x85EB_CA6B);
            h ^= h >> 16;
            h = h.wrapping_mul(0x85EB_CA6B);
            h ^= h >> 13;
            ((h as f32) / (u32::MAX as f32)) * 2.0 - 1.0
        }
    }

    fn share(familiarity: f32, want: Deviation) -> f32 {
        let (mut hits, mut total) = (0.0f32, 0.0f32);
        for cz in 0..60 {
            for cx in 0..60 {
                total += 1.0;
                if deviation_at(&TestNoise, 42, cx, cz, familiarity) == want {
                    hits += 1.0;
                }
            }
        }
        hits / total
    }

    #[test]
    fn a_world_has_one_signature_everywhere() {
        // The motif is a property of the world, not of where you stand.
        let a = motif_for(42);
        for seed in [42u32; 8] {
            assert_eq!(motif_for(seed), a);
        }
        // ...and different worlds genuinely differ.
        let signatures: std::collections::HashSet<_> =
            (0..64u32).map(|s| motif_for(s).signature).collect();
        assert!(
            signatures.len() >= 3,
            "seeds collapsed onto {} signatures",
            signatures.len()
        );
    }

    #[test]
    fn normalcy_dominates_so_that_deviation_can_land() {
        // If most rooms were not the habitual room, nothing would read as a
        // departure from anything.
        assert!(
            share(0.0, Deviation::Habitual) > 0.75,
            "an unfamiliar world should be overwhelmingly habitual, got {:.2}",
            share(0.0, Deviation::Habitual)
        );
    }

    #[test]
    fn familiarity_makes_the_level_answer_back() {
        let calm = share(0.0, Deviation::Variation) + share(0.0, Deviation::Break);
        let settled = share(1.0, Deviation::Variation) + share(1.0, Deviation::Break);
        assert!(
            settled > calm * 2.0,
            "settling in did not draw the level's attention ({settled:.2} vs {calm:.2})"
        );
    }

    #[test]
    fn shouting_stays_rare_however_settled_you_get() {
        // A thing that shouts constantly is loud, not frightening.
        assert!(
            share(1.0, Deviation::Exaggeration) < 0.06,
            "exaggeration became the norm at {:.3}",
            share(1.0, Deviation::Exaggeration)
        );
    }

    #[test]
    fn a_break_contradicts_rather_than_merely_differing() {
        // The break inverts the signature: that is what makes it the room
        // that disagrees with every other room, rather than another room.
        assert!(Deviation::Break.intensity() < 0.0);
        assert!(Deviation::Habitual.intensity() > 0.0);
        assert!(Deviation::Exaggeration.intensity() > Deviation::Variation.intensity());
    }

    #[test]
    fn deviation_is_deterministic() {
        for k in 0..50 {
            assert_eq!(
                deviation_at(&TestNoise, 7, k, k * 3, 0.4),
                deviation_at(&TestNoise, 7, k, k * 3, 0.4)
            );
        }
    }
}
