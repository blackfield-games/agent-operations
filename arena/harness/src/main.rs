//! arena-harness — a line-delimited JSON loopback gateway over the arena-02
//! reference core, so an external (e.g. Python) agent can play a real match
//! against the real simulation without an engine or a network.
//!
//! This is transport glue, NOT a second source of gameplay: every rule lives in
//! `arena-core` and is reached only through the existing `observe` / `ingest` /
//! `step`. The harness mints no gameplay state of its own. A real networked
//! gateway is one connection per seat; this multiplexes all seats over one stdio
//! pipe with a thin `{ "seat": u8, "frame": <arena-01 msg> }` envelope, so the
//! `frame` payload is pure arena-01 and an agent SDK written against it needs no
//! harness-specific code.
//!
//! Determinism: the match id and seed come from argv (a random server-minted id
//! would make the replay hash non-reproducible), so the same flags produce a
//! byte-identical `MatchResult` every run — the property the integration test pins.
//!
//! Protocol, per seat, exactly as arena-01 defines it:
//!   server -> Challenge ; agent -> Join ; server -> Welcome, Start ;
//!   during the pre-live countdown  server -> Observe (`phase == Starting`, broadcast — no reply) ;
//!   then each Live tick  server -> Observe ; agent -> Act|Leave ;
//!   and at the end  server -> End(MatchResult).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use arena_core::{
    arena_map, named_arena, ranked_delta, ranked_field_delta, settlement, AimMode, Match, Rules,
    SeatDelta, Settlement, WeaponMode, DEFAULT_RATING,
};
use arena_match::{
    JoinOutcome, JoinRequest, LadderSnapshot, MatchParams, Matchmaker, RegistrySnapshot,
    SignatureVerifier, SnapshotError,
};
use arena_proto::{
    check_version, verify_join_signature, ActionIntent, AgentMsg, GatewayMsg, JoinVerifyError,
    MatchConfig, MatchMode, MatchPhase, MatchResult, ReplayRecord, SeatId, SeatInfo, Vec2,
    POSITION_SCALE, PROTOCOL_VERSION,
};
use uuid::Uuid;

/// A fixed, valid v4-shaped id used when `--match-id` is omitted, so a bare
/// invocation is still fully deterministic.
const DEFAULT_MATCH_ID: &str = "00000000-0000-4000-8000-000000000000";

struct Args {
    match_id: Uuid,
    seed: u64,
    seats: u8,
    max_ticks: u64,
    /// Drive the off-chain settlement path through a [`MockSettler`] after the
    /// match (logging to stderr), mirroring mesh's `--relay-dev-mock`. Off by
    /// default so the loopback's stdout — and its replay determinism — is
    /// byte-identical; the live Base settler is operator-gated.
    settle_dev_mock: bool,
    /// When set, form the match through the `arena-match` [`Matchmaker`] under this
    /// [`MatchMode`] instead of seating the roster directly — so the Human/Agent/Mixed
    /// gating and authenticated ranked admission are exercised end to end. `None` (no
    /// `--mode`) is the pre-this-flag direct-seating path, byte-identical.
    mode: Option<MatchMode>,
    /// Seats that join as humans in `--mode mixed` (comma-separated). Only Mixed needs
    /// the hint: the arena-01 `Join` carries no controller kind, so a token-less join
    /// is otherwise a casual agent, and a Mixed match requires at least one of each. In
    /// `human` mode every seat is a human (a signed join is the agent intruder Mixed
    /// would admit), and in `agent` mode every seat is a ranked agent, so the list is
    /// consulted for Mixed only. Empty by default.
    human_seats: Vec<SeatId>,
    /// Persist and restore the matchmaker's ranked rating ladder across runs. When set,
    /// the ladder is SEEDED from this file at startup (a missing or empty file starts
    /// fresh — byte-identical to today; a present-but-corrupt one aborts the run loudly
    /// rather than silently resetting standings) and the POST-settle ladder is written
    /// back atomically after the match. Only a `--mode` run moves a ladder, so the flag
    /// is consulted on that path; `None` keeps the in-memory-only behaviour.
    ladder_file: Option<PathBuf>,
    /// On-chain-registered agent addresses eligible for a ranked seat, each supplied by a
    /// repeated `--registered <addr>` — the arena-side view of `AgentRegistry.isRegistered`
    /// the matchmaker gates ranked (`Agent`-mode) admission on. A ranked seat must present a
    /// valid signature (possession) AND claim an address in this set; a registered set that
    /// omits a signed ranked seat rejects it (it could never settle on-chain). Registration
    /// is a ranked concern only — the matchmaker never consults it for `Mixed` casual
    /// cross-play or `Human` seats. Empty (no `--registered`) leaves it UNENFORCED —
    /// byte-identical to the possession-only ranked path. Consulted on the `--mode` path.
    registered: Vec<String>,
    /// The builtin arena whose static geometry — vision blockers + world pickups — the
    /// match plays under, resolved through [`arena_map`]. Set by `--map <key>`; the
    /// default `""` is the empty arena (no occlusion, no items), byte-identical to the
    /// pre-this-flag harness. Applies to BOTH the direct and `--mode` paths, so a match
    /// reaches the named arena's cover + pickups (and an agent SDK receives them in
    /// [`GatewayMsg::Start`]) however the roster is formed.
    arena: &'static str,
    /// Perception-memory window in ticks (`Rules::perception_memory_ticks`): how long a
    /// seat remembers a lost entity's last-known position (surfaced as a `VisibleEntity`
    /// with `in_line_of_sight == false`). Set by `--perception-memory`; the default `0`
    /// disables memory, byte-identical to the pre-this-flag harness. Applies to BOTH the
    /// direct and `--mode` paths through [`rules_from`]: the matchmaker carries it on
    /// [`MatchParams::rules`], so a matchmade/ranked match forms under the same window a
    /// hand-seated one does.
    perception_memory: u16,
    /// Forward field-of-view cone as an octant spread (`Rules::fov_octant_spread`,
    /// `0..=4`): a seat perceives an in-range enemy only when its bearing is within this
    /// many octants of the seat's facing. Set by `--fov`; the default `4` is the full
    /// circle — omnidirectional, byte-identical to the pre-flag harness (and the replay
    /// digest). Applies to BOTH the direct and `--mode` paths through [`rules_from`]: the
    /// matchmaker carries it on [`MatchParams::rules`], so a matchmade/ranked match forms
    /// under the same cone a hand-seated one does.
    fov: u8,
    /// Fire-beam aim resolution (`Rules::aim_mode`): `octant` snaps the beam to the nearest of
    /// eight 45° octants, `fine` resolves it on the 64-way (5.625°) table so a sub-octant lead
    /// lands a shot the octant snap would miss. Set by `--aim-mode`; the default `octant` is
    /// byte-identical to the pre-flag harness (and the replay digest). Applies to BOTH the
    /// direct and `--mode` paths through [`rules_from`]: the matchmaker carries it on
    /// [`MatchParams::rules`], so a matchmade/ranked match forms under the same aim resolution
    /// a hand-seated one does.
    aim_mode: AimMode,
    /// Allow allied damage (`Rules::friendly_fire`): when set, a fire (beam, projectile, or
    /// melee swing) that crosses a same-team body damages it instead of passing through — the
    /// hit lands but never scores a kill for the shooter. A presence flag (`--friendly-fire`,
    /// no value, like `--settle-dev-mock`); the default `false` spares allies, byte-identical
    /// to the pre-flag harness (and the replay digest). Applies to BOTH the direct and `--mode`
    /// paths through [`rules_from`]: the matchmaker carries it on [`MatchParams::rules`], so a
    /// matchmade/ranked match forms under the same allied-damage rule a hand-seated one does.
    /// The effect surfaces only with teamed rosters — today's harness seats a free-for-all
    /// (every seat its own team), so the rule is dark until a teamed deployment configures it —
    /// but `friendly_fire` is a real `Rules` determinant folded into the digest.
    friendly_fire: bool,
    /// Downward gravity magnitude (`Rules::gravity`): `0` (the default) keeps vertical physics
    /// OFF — jumps are inert, every pawn `z` stays `0`, byte-identical to a 2D match (and its
    /// replay digest). A positive value turns jumping on (a grounded jump launches at the fixed
    /// `JUMP_VELOCITY` and this gravity pulls it back; higher ⇒ a lower, shorter arc). Set by
    /// `--gravity` as a non-negative magnitude — a negative is rejected at parse, since core
    /// gates vertical physics on `gravity > 0` so a negative is an inert footgun. Applies to
    /// BOTH the direct and `--mode` paths through [`rules_from`] via [`MatchParams::rules`]. On
    /// its own gravity leaves combat planar (outcome-identical): it unblocks the z-combat family
    /// for a configured deployment, but is not a HIT determinant until `vertical_hit_tolerance > 0`.
    gravity: i32,
    /// Pre-live spawn-countdown length (`Rules::starting_ticks`): `0` (the default) opens the match
    /// directly in `Live` at tick 0, byte-identical to the pre-countdown harness (and its replay
    /// digest, which the countdown never touches). A positive value opens the match in `Starting`
    /// and the pump burns that many countdown ticks — no action simulated, `tick` held at 0 —
    /// before `Live`. Set by `--starting-ticks` as a non-negative count. Applies to BOTH the direct
    /// and `--mode` paths through [`rules_from`] via [`MatchParams::rules`].
    starting_ticks: u32,
    /// How a fire press resolves (`Rules::weapon_mode`): `hitscan` is an instant beam that lands
    /// the tick it is fired (the default), `projectile` spawns a traveling shot that hits only
    /// when its swept path crosses a body on a later (or point-blank) tick, and `melee` is a
    /// close-quarters cleave striking every enemy in `melee_range` + the frontal arc. Set by
    /// `--weapon-mode`; the default `hitscan` is byte-identical to the pre-flag harness (and the
    /// replay digest). Applies to BOTH the direct and `--mode` paths through [`rules_from`] via
    /// [`MatchParams::rules`], so a matchmade/ranked match forms under the same weapon a
    /// hand-seated one does.
    weapon_mode: WeaponMode,
    /// The vertical band (`Rules::vertical_hit_tolerance`) within which a shot connects: a hit
    /// lands only when `|shooter_z - target_z| <= vertical_hit_tolerance`, gating beam, projectile,
    /// AND melee resolution alike. `0` (the default) DISABLES z-coupling — combat stays planar,
    /// byte-identical to the pre-flag harness (and the replay digest). A non-negative `i32` set by
    /// `--vertical-hit-tolerance`; it is the combat companion to `--gravity` (gravity arcs pawns in
    /// `z`, this decides whether that `z` gap matters to a shot). Applies to BOTH the direct and
    /// `--mode` paths through [`rules_from`] via [`MatchParams::rules`].
    vertical_hit_tolerance: i32,
    /// Hard-landing damage magnitude (`Rules::fall_damage`): the HP a landing deals once a
    /// falling pawn's downward impact speed exceeds `fall_damage_threshold`. `0` (the default)
    /// keeps every landing safe — no landing damages — byte-identical to the pre-flag harness
    /// (and the replay digest). A `u16` set by `--fall-damage`; it bites only once `gravity > 0`
    /// actually drops pawns AND a landing clears the threshold, so on a 2D field it is inert.
    /// Applies to BOTH the direct and `--mode` paths through [`rules_from`] via
    /// [`MatchParams::rules`]. The companion `fall_damage_threshold` (the speed gate) is a
    /// separate knob.
    fall_damage: u16,
    /// Upward `z` impulse (`Rules::knockback_velocity`) a landed damaging hit imparts to the
    /// SURVIVING target — the variable-fall-height source. `0` (the default) imparts no impulse,
    /// byte-identical to the pre-flag harness (and the replay digest). A non-negative `i32` set by
    /// `--knockback-velocity` — a negative is rejected at parse (it would launch the target DOWNWARD
    /// into the floor, an inert footgun). Like `gravity` it bites only with `gravity > 0` (the sole
    /// source of any non-zero `z`); with gravity off the impulse is suppressed. Applies to BOTH the
    /// direct and `--mode` paths through [`rules_from`] via [`MatchParams::rules`]. The companion
    /// `knockback_horizontal` (the planar shove, no gravity needed) is a separate knob.
    knockback_velocity: i32,
    /// Slide a grazing move along a blocker instead of dead-stopping (`Rules::wall_slide`): when set,
    /// a diagonal step whose full path is refused by a blocker retries each axis independently and
    /// keeps the component that clears, so the pawn slides along the surface. A presence flag
    /// (`--wall-slide`, no value, like `--friendly-fire`); the default `false` stops the move at its
    /// origin, byte-identical to the pre-flag harness (and the replay digest). Applies to BOTH the
    /// direct and `--mode` paths through [`rules_from`]: the matchmaker carries it on
    /// [`MatchParams::rules`], so a matchmade/ranked match forms under the same movement rule a
    /// hand-seated one does. The effect surfaces only when a move actually grazes a blocker — the
    /// empty default arena has none to graze, so the rule is dark until a `--map` with cover is
    /// configured — but `wall_slide` is a real `Rules` determinant folded into the digest.
    wall_slide: bool,
    /// Impact-speed gate (`Rules::fall_damage_threshold`) a landing must EXCEED to take `fall_damage`:
    /// a landing wounds only when its downward impact speed `> fall_damage_threshold`, so this is the
    /// speed companion to the `fall_damage` magnitude — it decides WHICH landings are "hard". `0` (the
    /// default) gates nothing — every landing with any downward impact takes the full `fall_damage`,
    /// byte-identical to the pre-flag harness (and the replay digest); it is fully inert while
    /// `fall_damage == 0`. A non-negative `i32` set by `--fall-damage-threshold` — a negative is
    /// rejected at parse (core compares `impact > threshold`, so a negative would make EVERY landing
    /// wound, the inverse of raising the bar). Applies to BOTH the direct and `--mode` paths through
    /// [`rules_from`] via [`MatchParams::rules`].
    fall_damage_threshold: i32,
    /// Planar knockback shove (`Rules::knockback_horizontal`): the position units a landed damaging hit
    /// displaces the SURVIVING target along the shooter→target octant, through the same `slide()` a walk
    /// uses (so the shove stops AT a wall and never tunnels or leaves the arena). The horizontal sibling
    /// of `knockback_velocity` — but UNLIKE the vertical impulse it needs NO gravity: a planar shove is
    /// meaningful in a 2D match (core gates it on `knockback_horizontal > 0` alone). `0` (the default)
    /// imparts no shove — the target's position is unchanged on a hit, byte-identical to the pre-flag
    /// harness (and the replay digest). A non-negative `i32` set by `--knockback-horizontal` — a negative
    /// is rejected at parse (core's `> 0` gate makes it inert, a silent footgun). Applies to BOTH the
    /// direct and `--mode` paths through [`rules_from`] via [`MatchParams::rules`].
    knockback_horizontal: i32,
    /// Dash rate-gate + on/off switch (`Rules::dash_cooldown`): the ticks that must elapse between
    /// `ability`-button dashes. `0` (the default) DISABLES the dash entirely — an ability press is
    /// inert, byte-identical to the pre-flag harness (and the replay digest). A positive `u16` set by
    /// `--dash-cooldown` turns it on: a grounded ability press with a move direction bursts the pawn the
    /// fixed `DASH_DISTANCE` along `move_dir` (bounds- and blocker-clamped like a step), then this many
    /// ticks must pass before the next dash. Applies to BOTH the direct and `--mode` paths through
    /// [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match forms under the same dash
    /// cadence a hand-seated one does. The burst DISTANCE is a fixed core constant, not a flag — this
    /// cadence is the only dash tuning the digest binds.
    dash_cooldown: u16,
    /// Pawn-vs-pawn occupancy radius (`Rules::pawn_radius`): the body disc every OTHER alive pawn
    /// presents in the slide path. `0` (the default) DISABLES occupancy — pawns are not obstacles to one
    /// another and a move may end on another pawn's cell, byte-identical to the pre-flag harness (and the
    /// replay digest). A positive `i32` set by `--pawn-radius` turns it on: a step whose swept path would
    /// bring the mover's centre WITHIN this distance of another alive pawn is refused (the mover holds at
    /// the step origin), gating the walk, the dash burst, AND a `knockback_horizontal` shove alike. A
    /// negative is rejected at parse — core gates occupancy on `pawn_radius > 0`, so a negative is INERT
    /// (a silent no-collision footgun) and `> i32::MAX` would wrap. Applies to BOTH the direct and
    /// `--mode` paths through [`rules_from`] via [`MatchParams::rules`]. The z-companion `pawn_height`
    /// (the vertical band that lets a jump vault a body) is a separate knob.
    pawn_radius: i32,
    /// Pawn occupancy body HEIGHT (`Rules::pawn_height`): the vertical band that z-couples
    /// `--pawn-radius`. `0` (the default) keeps occupancy PLANAR — a pawn's elevation is ignored, a pawn
    /// mid-jump still occupies its ground column, byte-identical to the pre-flag harness (and the replay
    /// digest). A positive `i32` set by `--pawn-height` couples `z`: another pawn's body blocks a step
    /// only when their XY discs overlap AND their feet are within `pawn_height` (each pawn a cylinder of
    /// this height), so a pawn that jumps higher than the band vaults the body instead of freezing. A
    /// negative is rejected at parse — core gates the band on `pawn_height > 0`, so a negative is INERT
    /// and `> i32::MAX` would wrap. Applies to BOTH the direct and `--mode` paths through [`rules_from`]
    /// via [`MatchParams::rules`]. Only meaningful with `--pawn-radius > 0` (no occupancy at all
    /// otherwise) AND `--gravity > 0` (the sole source of any non-zero `z`); with either off the band
    /// never changes an outcome — its reachability is the contract here.
    pawn_height: i32,
    /// Shield pool cap (`Rules::max_shield`): the ceiling on a pawn's damage-absorbing shield, which
    /// drains before health. `0` (the default) DISABLES shield — no pawn can hold any and a
    /// `PickupKind::Shield` is inert, byte-identical to the pre-flag harness (and the replay digest). A
    /// positive `u16` set by `--max-shield` turns shield pickups on: a pawn starts at `0` shield and
    /// earns it (capped here) by collecting a Shield pickup. Applies to BOTH the direct and `--mode`
    /// paths through [`rules_from`] via [`MatchParams::rules`]. The effect needs a Shield pickup to
    /// collect — the default arenas spawn none, so the cap is dark until a `--map` with shield pickups is
    /// configured — but `max_shield` is a real `Rules` determinant folded into the digest, so the pin is
    /// reachability.
    max_shield: u16,
    /// Starting (and max) health (`Rules::start_health`): the HP every pawn spawns with and reloads/heals
    /// cap toward. Set by `--start-health`; UNLIKE the feature-toggle knobs above (which default `0` =
    /// off) this is a base-balance value, so its default is `Rules::default().start_health` (100) — an
    /// absent flag is byte-identical to the pre-flag harness (and the replay digest) at the DEFAULT
    /// health, NOT at `0` (a `0`-health pawn spawns already-downed). A `u16` (the parse is the fence:
    /// a negative or `> 65535` aborts). Applies to BOTH the direct and `--mode` paths through
    /// [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match forms under the same health
    /// pool a hand-seated one does. Lower health ⇒ a faster time-to-kill (fewer shots down a pawn).
    start_health: u16,
    /// Damage one landed shot deals (`Rules::damage`): the HP a single connecting shot subtracts from a
    /// pawn. Set by `--damage`; UNLIKE the feature-toggle knobs above (which default `0` = off) this is a
    /// base-balance value, so its default is `Rules::default().damage` (25 — four shots down a full-health
    /// pawn) — an absent flag is byte-identical to the pre-flag harness (and the replay digest) at the
    /// DEFAULT damage, NOT at `0` (a `0`-damage shot can never down a pawn). A `u16` (the parse is the
    /// fence: a negative or `> 65535` aborts). Applies to BOTH the direct and `--mode` paths through
    /// [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match forms under the same per-shot
    /// damage a hand-seated one does. Lower damage ⇒ a slower time-to-kill (more shots to down a pawn). The
    /// HP-pool companion `start_health` (how many such shots a pawn survives) is a separate knob.
    damage: u16,
    /// Ticks a pawn must wait between ranged shots (`Rules::fire_cooldown`): the rate-of-fire gate the sim
    /// reloads after every beam/projectile (`pawn.cooldown = fire_cooldown`), so a higher value is a slower
    /// cadence (fewer shots/sec). Set by `--fire-cooldown`; UNLIKE the feature-toggle knobs above (which
    /// default `0` = off) this is a base-balance value, so its default is `Rules::default().fire_cooldown`
    /// (6 — five shots/sec at 30 Hz) — an absent flag is byte-identical to the pre-flag harness (and the
    /// replay digest) at the DEFAULT cadence, NOT at `0` (a `0`-cooldown pawn can fire EVERY tick, the
    /// degenerate unbounded-projectile-spawn case core's per-tick-work note warns against). A `u16` (the
    /// parse is the fence: a negative or `> 65535` aborts). Applies to BOTH the direct and `--mode` paths
    /// through [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match forms under the same
    /// fire cadence a hand-seated one does. The per-shot `damage` (how hard each of those shots lands) is a
    /// separate knob.
    fire_cooldown: u16,
    /// Magazine capacity (`Rules::mag_size`): the ammo a pawn spawns with, reloads to, and refills toward at
    /// an ammo pickup — and, since ranged fire gates on `ammo > 0`, the number of shots it can land before a
    /// reload. Set by `--mag-size`; UNLIKE the feature-toggle knobs above (which default `0` = off) this is a
    /// base-balance value, so its default is `Rules::default().mag_size` (30) — an absent flag is
    /// byte-identical to the pre-flag harness (and the replay digest) at the DEFAULT capacity, NOT at `0` (a
    /// `0`-mag pawn spawns empty and can never fire a ranged shot — it only melees). A `u16` (the parse is the
    /// fence: a negative or `> 65535` aborts). Applies to BOTH the direct and `--mode` paths through
    /// [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match forms under the same magazine a
    /// hand-seated one does. Smaller magazine ⇒ more frequent reloads (more ticks spent unable to fire).
    mag_size: u16,
    /// Max planar displacement per tick at full move intent, in position units (`Rules::max_speed`): the
    /// distance the sim's per-tick walk slides a pawn at full intent (`magnitude == max_speed`), so a higher
    /// value is a faster movement pace. Set by `--max-speed`; UNLIKE the feature-toggle knobs above (which
    /// default `0` = off) this is a base-balance value, so its default is `Rules::default().max_speed` (200 —
    /// 0.2 m/tick, ~6 m/s at 30 Hz) — an absent flag is byte-identical to the pre-flag harness (and the replay
    /// digest) at the DEFAULT pace, NOT at `0` (a `0`-speed pawn is frozen in place, unable to walk, dodge, or
    /// chase). A non-negative `i32` (the u32-then-`i32` fence in [`parse_max_speed`]: a negative — which core
    /// has no movement meaning for — or a value past `i32::MAX` aborts). Applies to BOTH the direct and
    /// `--mode` paths through [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match forms
    /// under the same movement pace a hand-seated one does.
    max_speed: i32,
    /// How far a seat perceives another entity, in position units (`Rules::perception_range`): an entity is
    /// observed only if it lies within this radius of the eye (the sim gates perception on `within(eye,
    /// target, perception_range)`), so a shorter range is less battlefield awareness. Set by
    /// `--perception-range`; UNLIKE the feature-toggle knobs above (which default `0` = off) this is a
    /// base-balance value, so its default is `Rules::default().perception_range` (40·`POSITION_SCALE`, 40 m)
    /// — an absent flag is byte-identical to the pre-flag harness (and the replay digest) at the DEFAULT
    /// range, NOT at `0` (a `0`-range seat is BLIND, perceiving no entity at any distance). A non-negative
    /// `i32` (the u32-then-`i32` fence in [`parse_perception_range`]: a negative — meaningless for a radius —
    /// or a value past `i32::MAX` aborts). Applies to BOTH the direct and `--mode` paths through
    /// [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match forms under the same perception
    /// radius a hand-seated one does.
    perception_range: i32,
    /// How far a beam-hitscan shot reaches, in position units (`Rules::weapon_range`): the sim resolves a
    /// hitscan hit only within this reach (`range2 = (weapon_range as i128).pow(2)`) and expires a traveling
    /// projectile once it has flown past it, so a shorter range is closer-quarters engagements. Set by
    /// `--weapon-range`; UNLIKE the feature-toggle knobs above (which default `0` = off) this is a
    /// base-balance value, so its default is `Rules::default().weapon_range` (30·`POSITION_SCALE`, 30 m) — an
    /// absent flag is byte-identical to the pre-flag harness (and the replay digest) at the DEFAULT reach, NOT
    /// at `0` (a `0`-range weapon reaches nothing, landing no ranged hit at any distance). A non-negative
    /// `i32` (the u32-then-`i32` fence in [`parse_weapon_range`]: a negative — meaningless for a reach — or a
    /// value past `i32::MAX` aborts). Applies to BOTH the direct and `--mode` paths through [`rules_from`] via
    /// [`MatchParams::rules`], so a matchmade/ranked match forms under the same weapon reach a hand-seated one
    /// does.
    weapon_range: i32,
    /// Lateral half-width of the hitscan beam, in position units (`Rules::hit_radius`): the aim tolerance that
    /// lets the coarse 8-way facing still land a shot, and in [`WeaponMode::Projectile`] the pawn-body
    /// half-width a swept shot must reach to hit, so a wider radius is more forgiving aim (more shots connect)
    /// and a tighter one demands sharper facing. Set by `--hit-radius`; UNLIKE the feature-toggle knobs above
    /// (which default `0` = off) this is a base-balance value, so its default is `Rules::default().hit_radius`
    /// (1500, a 1.5 m beam radius) — an absent flag is byte-identical to the pre-flag harness (and the replay
    /// digest) at the DEFAULT radius, NOT at `0` (a `0` radius is a needle-thin beam that lands only on a
    /// dead-centre target). A non-negative `i32` (the u32-then-`i32` fence in [`parse_hit_radius`]: a negative
    /// — meaningless for a half-width — or a value past `i32::MAX` aborts). Applies to BOTH the direct and
    /// `--mode` paths through [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match forms under
    /// the same aim tolerance a hand-seated one does.
    hit_radius: i32,
    /// Ticks between [`WeaponMode::Melee`] swings (`Rules::melee_cooldown`): the swing cadence, the melee twin
    /// of the ranged [`fire_cooldown`](Args::fire_cooldown), so a longer cooldown is a slower cleave and a
    /// shorter one a faster one (only read in [`WeaponMode::Melee`]). Set by `--melee-cooldown`; UNLIKE the
    /// feature-toggle knobs (which default `0` = off) this is a base-balance value, so its default is
    /// `Rules::default().melee_cooldown` (`default_melee_cooldown()` = 15 ticks, a slower cadence than the
    /// default ranged `fire_cooldown`) — an absent flag is byte-identical to the pre-flag harness (and the
    /// replay digest) at the DEFAULT cadence, NOT at `0` (a `0` cooldown swings EVERY tick, a continuous
    /// cleave). A `u16` (the plain `.parse()` rejects a negative and bounds the value), applied to BOTH the
    /// direct and `--mode` paths through [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match
    /// swings at the same cadence a hand-seated one does.
    melee_cooldown: u16,
    /// Damage one [`WeaponMode::Melee`] swing deals to each enemy it cleaves (`Rules::melee_damage`), clamped to
    /// the target's health (only read in [`WeaponMode::Melee`]). Set by `--melee-damage`; UNLIKE the
    /// feature-toggle knobs (which default `0` = off) this is a base-balance value, so its default is
    /// `Rules::default().melee_damage` (`default_melee_damage()` = 50, two swings down a full pawn) — an absent
    /// flag is byte-identical to the pre-flag harness (and the replay digest) at the DEFAULT damage, NOT at `0`
    /// (a `0`-damage swing never harms, a harmless melee pawn). A `u16` (the plain `.parse()` rejects a negative
    /// and bounds the value), applied to BOTH the direct and `--mode` paths through [`rules_from`] via
    /// [`MatchParams::rules`], so a matchmade/ranked match swings for the same damage a hand-seated one does.
    melee_damage: u16,
    /// Reach of a [`WeaponMode::Melee`] swing, in position units (`Rules::melee_range`): a cleave strikes EVERY
    /// enemy whose centre is within this distance (`range2 = (melee_range as i128).pow(2)`) AND inside the frontal
    /// arc, so a longer reach cleaves further and a shorter one demands closer contact (only read in
    /// [`WeaponMode::Melee`]). Set by `--melee-range`; UNLIKE the feature-toggle knobs (which default `0` = off)
    /// this is a base-balance value, so its default is `Rules::default().melee_range` (`default_melee_range()` =
    /// 2·`POSITION_SCALE`, a 2 m reach) — an absent flag is byte-identical to the pre-flag harness (and the replay
    /// digest) at the DEFAULT reach, NOT at `0` (a `0` reach cleaves only an enemy exactly on the shooter, a
    /// harmless melee pawn). A non-negative `i32` (the u32-then-`i32` fence in [`parse_melee_range`]: a negative —
    /// meaningless for a reach — or a value past `i32::MAX` aborts), applied to BOTH the direct and `--mode` paths
    /// through [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match cleaves to the same reach a
    /// hand-seated one does.
    melee_range: i32,
    /// Travel speed of a [`WeaponMode::Projectile`] shot, in position units per tick (`Rules::projectile_speed`):
    /// a fired projectile spawns and flies this far each tick along the firing octant, hitting only when its
    /// swept path crosses a body, so a faster shot closes the gap to a strafing target sooner and a slower one
    /// is easier to dodge over its flight (only read in [`WeaponMode::Projectile`]; core snaps it to the octant
    /// and clamps it to a per-tick bound at spawn). Set by `--projectile-speed`; UNLIKE the feature-toggle knobs
    /// (which default `0` = off) this is a base-balance value, so its default is `Rules::default().projectile_speed`
    /// (`default_projectile_speed()` = 2·`POSITION_SCALE`, 2 m/tick) — an absent flag is byte-identical to the
    /// pre-flag harness (and the replay digest) at the DEFAULT speed, NOT at `0` (a `0`-speed shot never leaves the
    /// muzzle and is force-expired by the termination backstop, landing no hit). A non-negative `i32` (the
    /// u32-then-`i32` fence in [`parse_projectile_speed`]: a negative — a projectile flies forward along the
    /// octant, never backward — or a value past `i32::MAX` aborts), applied to BOTH the direct and `--mode` paths
    /// through [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match flies shots at the same
    /// speed a hand-seated one does.
    projectile_speed: i32,
    /// Per-action wall-clock deadline in microseconds (`Rules::action_deadline_micros`): the time budget a seat
    /// has to return its action each tick — a tighter budget stresses an agent's compute, a looser one forgives a
    /// slow policy. This is the DECLARED budget: it is digest-bound (`canonical_encoding`) and carried on every
    /// [`GatewayMsg::Observe`]; the transport enforces it as a wall-clock read deadline (forfeiting a seat that
    /// misses it) only under [`Args::enforce_deadline`] — the default read is unbounded and blocking. Set by
    /// `--action-deadline-micros`; UNLIKE the feature-toggle knobs (which default `0` = off) this is a base-balance
    /// value, so its default is `Rules::default().action_deadline_micros` (`50_000`, 50 ms) — an absent flag is
    /// byte-identical to the pre-flag harness (and the replay digest) at the DEFAULT budget, NOT at `0` (a `0`
    /// budget gives a seat no time to act). A `u32` (the plain `.parse()` rejects a negative and bounds the value
    /// at `u32::MAX`), applied to BOTH the direct and `--mode` paths through [`rules_from`] via
    /// [`MatchParams::rules`], so a matchmade/ranked match declares the same deadline a hand-seated one does.
    action_deadline_micros: u32,
    /// Enforce [`Args::action_deadline_micros`] as a wall-clock READ deadline: when set, the live loop reads each
    /// tick's actions off a reader thread with `recv_timeout` against a shared per-tick budget, and a seat that
    /// misses it (or a closed stream) is omitted so the sim forfeits its tick. OFF by default — and off, the loop
    /// keeps the unbounded blocking read, so the golden/replay path stays timer-free and byte-identical (wall-clock
    /// enforcement is inherently non-deterministic and must never fire on the deterministic path). Opt-in via
    /// `--enforce-deadline`; inert (falls back to the blocking read) when the deadline is `0` — a `0`-µs budget has
    /// nothing to enforce. Applies to BOTH the direct and `--mode` live loops.
    enforce_deadline: bool,
    /// Heal/ammo collection radius in position units (`Rules::pickup_radius`): a pawn collects a pickup only when
    /// its centre is within this distance of the pickup, so a wider radius makes a pickup easier to grab and a
    /// tighter one demands closer contact. Set by `--pickup-radius`; UNLIKE the feature-toggle knobs (which
    /// default `0` = off) this is a base-balance value, so its default is `Rules::default().pickup_radius`
    /// (`default_pickup_radius()` = `POSITION_SCALE`, a 1 m contact disc) — an absent flag is byte-identical to
    /// the pre-flag harness (and the replay digest) at the DEFAULT radius, NOT at `0` (a `0` radius is collectible
    /// only by a pawn exactly on the pickup). A non-negative `i32` (the u32-then-`i32` fence in
    /// [`parse_pickup_radius`]: a negative — a squared distance is never `< 0`, so it is meaningless — or a value
    /// past `i32::MAX` aborts), applied to BOTH the direct and `--mode` paths through [`rules_from`] via
    /// [`MatchParams::rules`], so a matchmade/ranked match collects at the same radius a hand-seated one does.
    pickup_radius: i32,
    /// Ticks a collected pickup stays dormant before it respawns at its spawn point (`Rules::pickup_respawn_cooldown`):
    /// a deterministic per-tick countdown (no wall-clock) the sim arms when a pawn collects a pickup, so a longer
    /// cooldown keeps a contested heal/ammo spot empty longer (a real tempo decision) and a shorter one refreshes it
    /// sooner (a pickup-free match never consults it). Set by `--pickup-respawn-cooldown`; UNLIKE the feature-toggle
    /// knobs (which default `0` = off) this is a base-balance value, so its default is
    /// `Rules::default().pickup_respawn_cooldown` (`default_pickup_respawn_cooldown()` = 300 ticks, ~10 s at 30 Hz) —
    /// an absent flag is byte-identical to the pre-flag harness (and the replay digest) at the DEFAULT cooldown, NOT
    /// at `0` (a `0` cooldown respawns the pickup the tick after collection, so it is effectively always present — a
    /// real config an explicit `0` must forward). A `u16` (the plain `.parse()` rejects a negative and bounds the
    /// value), applied to BOTH the direct and `--mode` paths through [`rules_from`] via [`MatchParams::rules`], so a
    /// matchmade/ranked match respawns pickups on the same cooldown a hand-seated one does.
    pickup_respawn_cooldown: u16,
    /// Per-axis spawn jitter in position units (`Rules::spawn_jitter`): when the sim seats pawns around the opening
    /// ring it perturbs each seat by a PRNG draw in `[-spawn_jitter, +spawn_jitter]` per axis, so the seed scatters
    /// the opening (a wider jitter scatters it more) and a `0` jitter is a fully deterministic opening. Set by
    /// `--spawn-jitter`; UNLIKE the feature-toggle knobs (which default `0` = off) this is a base-balance value, so
    /// its default is `Rules::default().spawn_jitter` (`2 * POSITION_SCALE`, a 2 m per-axis jitter) — an absent flag
    /// is byte-identical to the pre-flag harness (and the replay digest) at the DEFAULT jitter, NOT at `0` (a `0`
    /// jitter is a deterministic opening with no per-seed perturbation — a real config an explicit `0` must forward).
    /// A non-negative `i32` (the u32-then-`i32` fence in [`parse_spawn_jitter`]: a negative — which would invert the
    /// jitter span — or a value past `i32::MAX` aborts), applied to BOTH the direct and `--mode` paths through
    /// [`rules_from`] via [`MatchParams::rules`], so a matchmade/ranked match scatters its opening the same way a
    /// hand-seated one does.
    spawn_jitter: i32,
    /// Half-width of the opening spawn line in position units (`Rules::spawn_radius`): the sim spreads the seats
    /// evenly across `[-spawn_radius, +spawn_radius]` on the X axis at match start (then `spawn_jitter` perturbs
    /// each), so a wider radius opens the seats farther apart and a `0` radius stacks every seat on the X origin
    /// (only the jitter then separates them). Set by `--spawn-radius`; UNLIKE the feature-toggle knobs (which
    /// default `0` = off) this is a base-balance value, so its default is `Rules::default().spawn_radius`
    /// (`20 * POSITION_SCALE`, a 20 m half-width) — an absent flag is byte-identical to the pre-flag harness (and
    /// the replay digest) at the DEFAULT half-width, NOT at `0` (a `0` radius stacks the seats on the origin — a
    /// real config an explicit `0` must forward). A non-negative `i32` (the u32-then-`i32` fence in
    /// [`parse_spawn_radius`]: a negative — which would invert the spread span — or a value past `i32::MAX` aborts),
    /// applied to BOTH the direct and `--mode` paths through [`rules_from`] via [`MatchParams::rules`], so a
    /// matchmade/ranked match opens its seats the same way a hand-seated one does.
    spawn_radius: i32,
}

/// Parse a `--mode` value into a [`MatchMode`]; the harness exposes the three
/// `arena-match` modes by their lowercase names.
fn parse_mode(value: &str) -> MatchMode {
    match value {
        "human" => MatchMode::Human,
        "agent" => MatchMode::Agent,
        "mixed" => MatchMode::Mixed,
        other => panic!("--mode is one of human|agent|mixed, got {other:?}"),
    }
}

/// Resolve a `--map` value to a builtin arena's canonical `'static` key, aborting on
/// an unknown one (mirroring [`parse_mode`]). The reject is loud and deliberate: an
/// unrecognised key would otherwise degrade through [`arena_map`] to the empty arena,
/// silently playing no-cover instead of the map the operator asked for.
fn parse_arena(value: &str) -> &'static str {
    named_arena(value).unwrap_or_else(|| panic!("--map names an unknown arena: {value:?}"))
}

/// Parse a `--fov` value to a forward-cone octant spread, rejecting anything outside the
/// sim's `0..=4` domain loudly (mirroring [`parse_mode`]/[`parse_arena`]). A spread `>4`
/// would saturate to the full circle in the sim — silently playing omnidirectional
/// perception instead of the cone the operator asked for — so the harness refuses it
/// rather than clamp.
fn parse_fov(value: &str) -> u8 {
    let spread: u8 = value.parse().expect("--fov is an octant spread (0..=4)");
    assert!(spread <= 4, "--fov is an octant spread in 0..=4 (4 = full circle), got {spread}");
    spread
}

/// Parse an `--aim-mode` value to a fire-beam resolution, rejecting an unknown name loudly
/// (mirroring [`parse_mode`]/[`parse_arena`]). `aim_mode` is a hit-resolution determinant —
/// it changes which shots connect — so a typo must abort, never silently default to `octant`
/// and mis-resolve combat.
fn parse_aim_mode(value: &str) -> AimMode {
    match value {
        "octant" => AimMode::Octant,
        "fine" => AimMode::Fine,
        other => panic!("--aim-mode is one of octant|fine, got {other:?}"),
    }
}

/// Parse a `--gravity` value into the downward magnitude [`Rules::gravity`] carries. Taken
/// as a `u32` then narrowed to `i32`: a negative is rejected at the CLI — arena-core gates
/// vertical physics on `gravity > 0`, so a negative would silently behave as `0` (off), a
/// footgun — and a magnitude past `i32::MAX` aborts rather than wrapping the fall integration.
fn parse_gravity(value: &str) -> i32 {
    let magnitude: u32 = value.parse().expect("--gravity is a non-negative integer (downward magnitude)");
    i32::try_from(magnitude).expect("--gravity exceeds the i32 range")
}

/// Parse a `--starting-ticks` value to the non-negative pre-live countdown length. A `u32` parse
/// rejects a negative outright — a countdown counts down, so a negative is meaningless.
fn parse_starting_ticks(value: &str) -> u32 {
    value.parse().expect("--starting-ticks is a non-negative integer (pre-live countdown ticks)")
}

/// Parse a `--vertical-hit-tolerance` value to the non-negative `z` band that gates a hit, the
/// same u32-then-i32 fence as [`parse_gravity`]: a negative would invert the `|z|` comparison and a
/// value past `i32::MAX` would wrap the band, so both abort before any spawn.
fn parse_vertical_hit_tolerance(value: &str) -> i32 {
    let band: u32 = value
        .parse()
        .expect("--vertical-hit-tolerance is a non-negative integer (elevation band)");
    i32::try_from(band).expect("--vertical-hit-tolerance exceeds the i32 range")
}

/// Parse a `--knockback-velocity` value to the non-negative upward `z` impulse [`Rules::knockback_velocity`]
/// carries, the same u32-then-i32 fence as [`parse_gravity`]: a negative would launch the hit target
/// DOWNWARD into the floor and a value past `i32::MAX` would wrap the impulse, so both abort before
/// any spawn.
fn parse_knockback_velocity(value: &str) -> i32 {
    let impulse: u32 = value
        .parse()
        .expect("--knockback-velocity is a non-negative integer (upward impulse)");
    i32::try_from(impulse).expect("--knockback-velocity exceeds the i32 range")
}

/// Parse a `--fall-damage-threshold` value to the non-negative impact-speed gate
/// [`Rules::fall_damage_threshold`] carries, the same u32-then-i32 fence as [`parse_gravity`]: core
/// compares `impact > threshold`, so a negative threshold would make EVERY landing wound (the inverse
/// of raising the bar) and a value past `i32::MAX` would wrap the gate — both abort before any spawn.
fn parse_fall_damage_threshold(value: &str) -> i32 {
    let gate: u32 = value
        .parse()
        .expect("--fall-damage-threshold is a non-negative integer (impact-speed gate)");
    i32::try_from(gate).expect("--fall-damage-threshold exceeds the i32 range")
}

/// Parse a `--knockback-horizontal` value to the non-negative planar shove [`Rules::knockback_horizontal`]
/// carries, the same u32-then-i32 fence as [`parse_gravity`]: core gates the shove on `knockback_horizontal
/// > 0`, so a negative is INERT (silently no shove — the operator dialed a pull and got nothing) and a value
/// past `i32::MAX` would wrap the displacement, so both abort before any spawn.
fn parse_knockback_horizontal(value: &str) -> i32 {
    let shove: u32 = value
        .parse()
        .expect("--knockback-horizontal is a non-negative integer (planar shove)");
    i32::try_from(shove).expect("--knockback-horizontal exceeds the i32 range")
}

/// Parse a `--pawn-radius` value to the non-negative occupancy radius [`Rules::pawn_radius`] carries,
/// the same u32-then-i32 fence as [`parse_gravity`]: core gates occupancy on `pawn_radius > 0`, so a
/// negative is INERT (silently no pawn-vs-pawn collision — the operator dialed a body and got a ghost)
/// and a value past `i32::MAX` would wrap the radius, so both abort before any spawn.
fn parse_pawn_radius(value: &str) -> i32 {
    let radius: u32 = value
        .parse()
        .expect("--pawn-radius is a non-negative integer (occupancy radius)");
    i32::try_from(radius).expect("--pawn-radius exceeds the i32 range")
}

/// Parse a `--pawn-height` value to the non-negative occupancy band [`Rules::pawn_height`] carries,
/// the same u32-then-i32 fence as [`parse_gravity`]: core gates the z-band on `pawn_height > 0`, so a
/// negative is INERT (silently planar occupancy) and a value past `i32::MAX` would wrap the band, so
/// both abort before any spawn.
fn parse_pawn_height(value: &str) -> i32 {
    let height: u32 = value
        .parse()
        .expect("--pawn-height is a non-negative integer (occupancy band)");
    i32::try_from(height).expect("--pawn-height exceeds the i32 range")
}

/// Parse a `--max-speed` value to the non-negative per-tick walk magnitude [`Rules::max_speed`] carries,
/// the same u32-then-i32 fence as [`parse_gravity`]: core slides a full-intent walk by `max_speed` position
/// units, so a negative has no movement meaning (core never walks a pawn backward by the cap) and a value
/// past `i32::MAX` would wrap the displacement, so both abort before any spawn.
fn parse_max_speed(value: &str) -> i32 {
    let magnitude: u32 = value
        .parse()
        .expect("--max-speed is a non-negative integer (per-tick walk magnitude)");
    i32::try_from(magnitude).expect("--max-speed exceeds the i32 range")
}

/// Parse a `--perception-range` value to the non-negative perception radius [`Rules::perception_range`]
/// carries, the same u32-then-i32 fence as [`parse_gravity`]: an entity is observed only if it lies
/// `within` this radius of the eye, so a negative radius is meaningless (it would perceive nothing) and a
/// value past `i32::MAX` would wrap the radius, so both abort before any spawn.
fn parse_perception_range(value: &str) -> i32 {
    let radius: u32 = value
        .parse()
        .expect("--perception-range is a non-negative integer (perception radius)");
    i32::try_from(radius).expect("--perception-range exceeds the i32 range")
}

/// Parse a `--weapon-range` value to the non-negative beam reach [`Rules::weapon_range`] carries, the same
/// u32-then-i32 fence as [`parse_gravity`]: a hitscan shot resolves a hit only `within` this reach and a
/// traveling projectile expires past it, so a negative reach is meaningless (it would land no hit) and a
/// value past `i32::MAX` would wrap the reach, so both abort before any spawn.
fn parse_weapon_range(value: &str) -> i32 {
    let reach: u32 = value
        .parse()
        .expect("--weapon-range is a non-negative integer (beam reach)");
    i32::try_from(reach).expect("--weapon-range exceeds the i32 range")
}

/// Parse a `--hit-radius` value to the non-negative beam half-width [`Rules::hit_radius`] carries, the same
/// u32-then-i32 fence as [`parse_weapon_range`]: the sim resolves a hit only within this lateral tolerance of
/// the beam (and treats it as the pawn-body half-width a projectile must reach), so a negative half-width is
/// meaningless (it lands no hit) and a value past `i32::MAX` would wrap it, so both abort before any spawn.
fn parse_hit_radius(value: &str) -> i32 {
    let radius: u32 = value
        .parse()
        .expect("--hit-radius is a non-negative integer (beam half-width)");
    i32::try_from(radius).expect("--hit-radius exceeds the i32 range")
}

/// Parse a `--melee-range` value to the non-negative cleave reach [`Rules::melee_range`] carries, the same
/// u32-then-i32 fence as [`parse_weapon_range`]: a swing strikes only enemies `within` this reach (and inside
/// the frontal arc), so a negative reach is meaningless (it would cleave nothing) and a value past `i32::MAX`
/// would wrap the reach, so both abort before any spawn.
fn parse_melee_range(value: &str) -> i32 {
    let reach: u32 = value
        .parse()
        .expect("--melee-range is a non-negative integer (cleave reach)");
    i32::try_from(reach).expect("--melee-range exceeds the i32 range")
}

/// Parse a `--projectile-speed` value to the non-negative travel speed [`Rules::projectile_speed`] carries, the
/// same u32-then-i32 fence as [`parse_melee_range`]: a projectile flies forward along the firing octant at this
/// per-tick speed, so a negative is meaningless (core never flies a shot backward) and a value past `i32::MAX`
/// would wrap the speed, so both abort before any spawn.
fn parse_projectile_speed(value: &str) -> i32 {
    let speed: u32 = value
        .parse()
        .expect("--projectile-speed is a non-negative integer (per-tick travel speed)");
    i32::try_from(speed).expect("--projectile-speed exceeds the i32 range")
}

/// Parse a `--pickup-radius` value to the non-negative collection radius [`Rules::pickup_radius`] carries, the
/// same u32-then-i32 fence as [`parse_hit_radius`]: a pawn collects a pickup only `within` this radius, so a
/// negative is meaningless (a squared distance is never `< 0`) and a value past `i32::MAX` would wrap the radius,
/// so both abort before any spawn.
fn parse_pickup_radius(value: &str) -> i32 {
    let radius: u32 = value
        .parse()
        .expect("--pickup-radius is a non-negative integer (collection radius)");
    i32::try_from(radius).expect("--pickup-radius exceeds the i32 range")
}

/// Parse a `--spawn-jitter` value to the non-negative per-axis jitter [`Rules::spawn_jitter`] carries, the same
/// u32-then-i32 fence as [`parse_pickup_radius`]: the PRNG perturbs each seat's opening position by a draw in
/// `[-spawn_jitter, +spawn_jitter]` per axis, so a negative is meaningless (it would invert the jitter span) and
/// a value past `i32::MAX` would wrap the span, so both abort before any spawn.
fn parse_spawn_jitter(value: &str) -> i32 {
    let jitter: u32 = value
        .parse()
        .expect("--spawn-jitter is a non-negative integer (per-axis spawn jitter)");
    i32::try_from(jitter).expect("--spawn-jitter exceeds the i32 range")
}

/// Parse a `--spawn-radius` value to the non-negative half-width [`Rules::spawn_radius`] carries, the same
/// u32-then-i32 fence as [`parse_spawn_jitter`]: the sim spreads the seats across `[-spawn_radius, +spawn_radius]`
/// on the X axis at the opening, so a negative is meaningless (it would invert the spread span) and a value past
/// `i32::MAX` would wrap the half-width, so both abort before any spawn.
fn parse_spawn_radius(value: &str) -> i32 {
    let radius: u32 = value
        .parse()
        .expect("--spawn-radius is a non-negative integer (spawn-line half-width)");
    i32::try_from(radius).expect("--spawn-radius exceeds the i32 range")
}

/// Parse a `--weapon-mode` value to the fire-resolution kind, rejecting an unknown name loudly
/// (mirroring [`parse_aim_mode`]). `weapon_mode` decides how a fire press resolves — instant
/// beam hitscan, a traveling projectile, or a melee cleave — so a typo must abort, never
/// silently default to `hitscan` and resolve a different weapon than the operator asked for.
fn parse_weapon_mode(value: &str) -> WeaponMode {
    match value {
        "hitscan" => WeaponMode::Hitscan,
        "projectile" => WeaponMode::Projectile,
        "melee" => WeaponMode::Melee,
        other => panic!("--weapon-mode is one of hitscan|projectile|melee, got {other:?}"),
    }
}

fn parse_args() -> Args {
    parse_args_from(std::env::args().skip(1))
}

/// The argv parse loop over the post-`argv[0]` tokens, taken as an iterator so it is
/// unit-testable with a synthetic stream (`parse_args` feeds it the real env args). A
/// presence flag (`--settle-dev-mock`, `--friendly-fire`) flips its bool WITHOUT consuming
/// the next token; a value flag pulls exactly one `it.next()`.
fn parse_args_from(args: impl Iterator<Item = String>) -> Args {
    let mut match_id = DEFAULT_MATCH_ID.to_string();
    let mut seed: u64 = 0;
    let mut seats: u8 = 2;
    let mut max_ticks: u64 = 3600;
    let mut settle_dev_mock = false;
    let mut mode: Option<MatchMode> = None;
    let mut human_seats: Vec<SeatId> = Vec::new();
    let mut ladder_file: Option<PathBuf> = None;
    let mut registered: Vec<String> = Vec::new();
    let mut arena: &'static str = "";
    let mut perception_memory: u16 = 0;
    let mut fov: u8 = 4;
    let mut aim_mode = AimMode::Octant;
    let mut friendly_fire = false;
    let mut gravity: i32 = 0;
    let mut starting_ticks: u32 = 0;
    let mut weapon_mode = WeaponMode::Hitscan;
    let mut vertical_hit_tolerance: i32 = 0;
    let mut fall_damage: u16 = 0;
    let mut knockback_velocity: i32 = 0;
    let mut wall_slide = false;
    let mut fall_damage_threshold: i32 = 0;
    let mut knockback_horizontal: i32 = 0;
    let mut dash_cooldown: u16 = 0;
    let mut pawn_radius: i32 = 0;
    let mut pawn_height: i32 = 0;
    let mut max_shield: u16 = 0;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 here would spawn
    // every pawn already-downed, NOT reproduce the pre-flag harness.
    let mut start_health: u16 = Rules::default().start_health;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0-damage shot could
    // never down a pawn, NOT reproduce the pre-flag harness.
    let mut damage: u16 = Rules::default().damage;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0-cooldown pawn fires
    // every tick (the degenerate unbounded-spawn case), NOT the pre-flag harness.
    let mut fire_cooldown: u16 = Rules::default().fire_cooldown;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0-mag pawn spawns empty
    // and can never fire a ranged shot, NOT the pre-flag harness.
    let mut mag_size: u16 = Rules::default().mag_size;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0-speed pawn is frozen
    // in place (unable to walk, dodge, or chase), NOT the pre-flag harness.
    let mut max_speed: i32 = Rules::default().max_speed;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0-range seat is BLIND
    // (perceives no entity at any distance), NOT the pre-flag harness.
    let mut perception_range: i32 = Rules::default().perception_range;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0-range weapon reaches
    // nothing (lands no ranged hit at any distance), NOT the pre-flag harness.
    let mut weapon_range: i32 = Rules::default().weapon_range;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 hit_radius is a
    // needle-thin beam that lands only on a dead-centre target, NOT the pre-flag harness.
    let mut hit_radius: i32 = Rules::default().hit_radius;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 melee_cooldown swings
    // every tick (a continuous cleave), NOT the pre-flag harness.
    let mut melee_cooldown: u16 = Rules::default().melee_cooldown;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 melee_damage swing
    // never harms (a harmless melee pawn), NOT the pre-flag harness.
    let mut melee_damage: u16 = Rules::default().melee_damage;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 melee_range cleaves
    // only an enemy exactly on the shooter (a harmless melee pawn), NOT the pre-flag harness.
    let mut melee_range: i32 = Rules::default().melee_range;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 projectile_speed shot
    // never leaves the muzzle and is force-expired landing no hit, NOT the pre-flag harness.
    let mut projectile_speed: i32 = Rules::default().projectile_speed;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 action_deadline_micros
    // gives a seat no time to act, NOT the pre-flag harness.
    let mut action_deadline_micros: u32 = Rules::default().action_deadline_micros;
    // Opt-in: enforcement of the action deadline as a wall-clock read budget. Off keeps the blocking read, so the
    // golden/replay path is timer-free and byte-identical.
    let mut enforce_deadline = false;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 pickup_radius is
    // collectible only by a pawn exactly on the pickup, NOT the pre-flag harness.
    let mut pickup_radius: i32 = Rules::default().pickup_radius;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 pickup_respawn_cooldown
    // respawns the pickup the tick after collection (effectively always present), NOT the pre-flag harness.
    let mut pickup_respawn_cooldown: u16 = Rules::default().pickup_respawn_cooldown;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 spawn_jitter is a fully
    // deterministic opening with no per-seed perturbation, NOT the pre-flag harness.
    let mut spawn_jitter: i32 = Rules::default().spawn_jitter;
    // Base-balance knob: its absent-default is the Rules default (non-zero), not 0 — a 0 spawn_radius stacks every
    // seat on the X origin (only spawn_jitter then separates them), NOT the pre-flag harness.
    let mut spawn_radius: i32 = Rules::default().spawn_radius;
    let mut it = args;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--match-id" => match_id = it.next().expect("--match-id needs a value"),
            "--seed" => seed = it.next().expect("--seed needs a value").parse().expect("seed is a u64"),
            "--seats" => seats = it.next().expect("--seats needs a value").parse().expect("seats is a u8"),
            "--max-ticks" => {
                max_ticks = it.next().expect("--max-ticks needs a value").parse().expect("max-ticks is a u64")
            }
            "--settle-dev-mock" => settle_dev_mock = true,
            "--mode" => mode = Some(parse_mode(&it.next().expect("--mode needs a value"))),
            "--human-seats" => {
                let v = it.next().expect("--human-seats needs a value");
                human_seats = v
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse().expect("each --human-seats entry is a u8"))
                    .collect();
            }
            "--ladder-file" => ladder_file = Some(it.next().expect("--ladder-file needs a value").into()),
            "--registered" => {
                let addr = it.next().expect("--registered needs an agent address");
                let addr = addr.trim();
                assert!(!addr.is_empty(), "--registered needs a non-empty agent address");
                registered.push(addr.to_string());
            }
            "--map" => arena = parse_arena(&it.next().expect("--map needs a value")),
            "--perception-memory" => {
                perception_memory = it
                    .next()
                    .expect("--perception-memory needs a value")
                    .parse()
                    .expect("perception-memory is a u16 (ticks)")
            }
            "--fov" => fov = parse_fov(&it.next().expect("--fov needs a value")),
            "--aim-mode" => aim_mode = parse_aim_mode(&it.next().expect("--aim-mode needs a value")),
            "--friendly-fire" => friendly_fire = true,
            "--gravity" => gravity = parse_gravity(&it.next().expect("--gravity needs a value")),
            "--starting-ticks" => {
                starting_ticks = parse_starting_ticks(&it.next().expect("--starting-ticks needs a value"))
            }
            "--weapon-mode" => {
                weapon_mode = parse_weapon_mode(&it.next().expect("--weapon-mode needs a value"))
            }
            "--vertical-hit-tolerance" => {
                vertical_hit_tolerance = parse_vertical_hit_tolerance(
                    &it.next().expect("--vertical-hit-tolerance needs a value"),
                )
            }
            "--fall-damage" => {
                fall_damage = it
                    .next()
                    .expect("--fall-damage needs a value")
                    .parse()
                    .expect("fall-damage is a u16 (hp)")
            }
            "--knockback-velocity" => {
                knockback_velocity =
                    parse_knockback_velocity(&it.next().expect("--knockback-velocity needs a value"))
            }
            "--wall-slide" => wall_slide = true,
            "--fall-damage-threshold" => {
                fall_damage_threshold = parse_fall_damage_threshold(
                    &it.next().expect("--fall-damage-threshold needs a value"),
                )
            }
            "--knockback-horizontal" => {
                knockback_horizontal =
                    parse_knockback_horizontal(&it.next().expect("--knockback-horizontal needs a value"))
            }
            "--dash-cooldown" => {
                dash_cooldown = it
                    .next()
                    .expect("--dash-cooldown needs a value")
                    .parse()
                    .expect("dash-cooldown is a u16 (ticks)")
            }
            "--pawn-radius" => {
                pawn_radius = parse_pawn_radius(&it.next().expect("--pawn-radius needs a value"))
            }
            "--pawn-height" => {
                pawn_height = parse_pawn_height(&it.next().expect("--pawn-height needs a value"))
            }
            "--max-shield" => {
                max_shield = it
                    .next()
                    .expect("--max-shield needs a value")
                    .parse()
                    .expect("max-shield is a u16 (shield cap)")
            }
            "--start-health" => {
                start_health = it
                    .next()
                    .expect("--start-health needs a value")
                    .parse()
                    .expect("start-health is a u16 (hp)")
            }
            "--damage" => {
                damage = it
                    .next()
                    .expect("--damage needs a value")
                    .parse()
                    .expect("damage is a u16 (hp)")
            }
            "--fire-cooldown" => {
                fire_cooldown = it
                    .next()
                    .expect("--fire-cooldown needs a value")
                    .parse()
                    .expect("fire-cooldown is a u16 (ticks)")
            }
            "--mag-size" => {
                mag_size = it
                    .next()
                    .expect("--mag-size needs a value")
                    .parse()
                    .expect("mag-size is a u16 (ammo)")
            }
            "--max-speed" => {
                max_speed = parse_max_speed(&it.next().expect("--max-speed needs a value"))
            }
            "--perception-range" => {
                perception_range =
                    parse_perception_range(&it.next().expect("--perception-range needs a value"))
            }
            "--weapon-range" => {
                weapon_range = parse_weapon_range(&it.next().expect("--weapon-range needs a value"))
            }
            "--hit-radius" => {
                hit_radius = parse_hit_radius(&it.next().expect("--hit-radius needs a value"))
            }
            "--melee-cooldown" => {
                melee_cooldown = it
                    .next()
                    .expect("--melee-cooldown needs a value")
                    .parse()
                    .expect("melee-cooldown is a u16 (ticks between swings)")
            }
            "--melee-damage" => {
                melee_damage = it
                    .next()
                    .expect("--melee-damage needs a value")
                    .parse()
                    .expect("melee-damage is a u16 (damage per swing)")
            }
            "--melee-range" => {
                melee_range = parse_melee_range(&it.next().expect("--melee-range needs a value"))
            }
            "--projectile-speed" => {
                projectile_speed =
                    parse_projectile_speed(&it.next().expect("--projectile-speed needs a value"))
            }
            "--action-deadline-micros" => {
                action_deadline_micros = it
                    .next()
                    .expect("--action-deadline-micros needs a value")
                    .parse()
                    .expect("action-deadline-micros is a u32 (microseconds)")
            }
            "--enforce-deadline" => enforce_deadline = true,
            "--pickup-radius" => {
                pickup_radius = parse_pickup_radius(&it.next().expect("--pickup-radius needs a value"))
            }
            "--pickup-respawn-cooldown" => {
                pickup_respawn_cooldown = it
                    .next()
                    .expect("--pickup-respawn-cooldown needs a value")
                    .parse()
                    .expect("pickup-respawn-cooldown is a u16 (ticks)")
            }
            "--spawn-jitter" => {
                spawn_jitter = parse_spawn_jitter(&it.next().expect("--spawn-jitter needs a value"))
            }
            "--spawn-radius" => {
                spawn_radius = parse_spawn_radius(&it.next().expect("--spawn-radius needs a value"))
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    Args {
        match_id: Uuid::parse_str(&match_id).expect("--match-id is a valid UUID"),
        seed,
        seats,
        max_ticks,
        settle_dev_mock,
        mode,
        human_seats,
        ladder_file,
        registered,
        arena,
        perception_memory,
        fov,
        aim_mode,
        friendly_fire,
        gravity,
        starting_ticks,
        weapon_mode,
        vertical_hit_tolerance,
        fall_damage,
        knockback_velocity,
        wall_slide,
        fall_damage_threshold,
        knockback_horizontal,
        dash_cooldown,
        pawn_radius,
        pawn_height,
        max_shield,
        start_health,
        damage,
        fire_cooldown,
        mag_size,
        max_speed,
        perception_range,
        weapon_range,
        hit_radius,
        melee_cooldown,
        melee_damage,
        melee_range,
        projectile_speed,
        action_deadline_micros,
        enforce_deadline,
        pickup_radius,
        pickup_respawn_cooldown,
        spawn_jitter,
        spawn_radius,
    }
}

/// A deterministic per-seat challenge nonce. Unranked play ignores it (no
/// signature), but it stays fixed per (match, seat) so the handshake adds no
/// nondeterminism.
fn nonce_for(match_id: Uuid, seat: SeatId) -> String {
    let mut bytes = match_id.as_bytes().to_vec();
    bytes.push(seat);
    hex::encode(bytes)
}

/// The harness's ranked-admission gate — the loopback twin of `arena-match`'s
/// production identity verifier and the networked Gateway. An EMPTY `signature_hex`
/// is an unranked seat (the baseline's default — admitted with no proof); a
/// non-empty one MUST recover to `agent_id` over `nonce` through the same
/// [`verify_join_signature`] the contract-backed admission uses. So the loopback
/// admits unranked play AND validly-signed ranked play, and refuses only a
/// PRESENTED-but-invalid signature — it never silently seats a forged identity.
///
/// Returns the RECOVERED identity the seat proved possession of: `Some(agent_id)` for
/// an admitted ranked seat — `verify_join_signature` succeeds only when the recovered
/// signer's address equals the claim, so the claim IS the verified identity — and
/// `None` for an unranked seat (no key, nothing to recover). The caller seats a ranked
/// seat under this address so settlement credits the real identity, not the roster label.
fn admit_join(agent_id: &str, nonce: &[u8], signature_hex: &str) -> Result<Option<String>, JoinVerifyError> {
    if signature_hex.is_empty() {
        return Ok(None);
    }
    verify_join_signature(PROTOCOL_VERSION, agent_id, nonce, signature_hex)?;
    Ok(Some(agent_id.to_owned()))
}

/// Overlay the handshake-recovered ranked identities onto the seated roster so a match
/// settles to the address each ranked seat PROVED it controls, not the pre-built
/// `agent-{i}` stand-in. Each `(seat, address)` came from [`admit_join`] returning
/// `Some` for a verified signature; an unranked seat has no entry and keeps its roster
/// label. Only the `controller` LABEL changes — `seat` and `team` stay index-driven, so
/// seat order, team assignment, and reproducibility are untouched; the only effect is
/// that [`settle_match`]/[`settle_field_match`], which credit `SeatInfo.controller`,
/// resolve the verified identity.
fn seat_recovered_identities(seats: &mut [SeatInfo], recovered: &[(SeatId, String)]) {
    for (seat, address) in recovered {
        if let Some(s) = seats.iter_mut().find(|s| s.seat == *seat) {
            s.controller = address.clone();
        }
    }
}

fn emit(out: &mut impl Write, seat: SeatId, msg: &GatewayMsg) {
    let frame = serde_json::to_value(msg).expect("serialize gateway message");
    let envelope = serde_json::json!({ "seat": seat, "frame": frame });
    writeln!(out, "{}", serde_json::to_string(&envelope).expect("serialize envelope"))
        .expect("write frame");
}

fn read_agent(line: &str) -> (SeatId, AgentMsg) {
    let v: serde_json::Value = serde_json::from_str(line).expect("parse transport envelope");
    let seat = v.get("seat").and_then(serde_json::Value::as_u64).expect("envelope seat") as SeatId;
    let msg: AgentMsg =
        serde_json::from_value(v.get("frame").expect("envelope frame").clone()).expect("parse agent message");
    (seat, msg)
}

fn next_line(lines: &mut impl Iterator<Item = io::Result<String>>) -> String {
    loop {
        match lines.next() {
            Some(Ok(l)) if !l.trim().is_empty() => return l,
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("read error on agent stream: {e}"),
            None => panic!("agent stream ended before the match did"),
        }
    }
}

/// Why a settlement submission recorded no fresh resolution.
#[derive(Debug)]
enum SettleError {
    /// This `match_id` is already resolved — the on-chain `MatchSettlement` fence
    /// (every resolution requires `Status.Open`) reverted `MatchNotOpen`. An
    /// *idempotent* outcome: a crash/retry after the match ended re-submitted a
    /// settlement that already landed. The caller treats it as benign (the terminal
    /// state already holds) instead of double-applying reputation or escrow.
    AlreadyResolved,
    /// The result is not a 1v1 ranked pair (returned by [`settle_match`]). `settle`/
    /// `settleDraw` take exactly two agents (`agentA` vs `agentB`), so any other seat
    /// count has no `settle`/`settleDraw` form — a 3+ field settles through
    /// [`settle_field_match`]/`settleField` instead — and the 1v1 seam refuses it rather
    /// than commit a resolution the contract can't accept.
    NotRankedPair,
    /// The match is not a multi-seat (FFA / 3+) ranked field. The symmetric guard to
    /// [`NotRankedPair`](SettleError::NotRankedPair) on the field seam
    /// ([`settle_field_match`]): a 1v1 pair (or a degenerate single/empty result)
    /// settles through [`settle_match`] in the single-delta `settle`/`settleDraw`
    /// shape, so the field path refuses it rather than emit a per-seat vector for a
    /// result the 1v1 path owns (and the contract's `settleField` itself rejects a
    /// sub-2 field).
    NotRankedField,
    /// The supplied per-seat ratings do not align 1:1 with the result's seats. The
    /// field delta pairs `ratings[i]` to `outcomes[i].seat` positionally, so a
    /// wrong-length rating vector would mis-pair seats to ratings — refused before any
    /// emit rather than settled against a misaligned vector.
    RatingsMismatch,
}

/// The off-chain → on-chain settlement boundary for a finished match. Mirrors the
/// three `MatchSettlement` resolutions — `settle` (decisive winner), `settleDraw`,
/// and `cancelMatch` — and, except for a cancel, commits the canonical
/// [`ReplayRecord`] digest of the exact match being settled.
///
/// The trait is transport-agnostic: it takes plain data, never a key, RPC URL, or
/// signer, so [`MockSettler`] drives the whole flow offline. The live Base
/// implementation (an RPC provider plus an authorized attester key with gas and
/// real-fund custody) is operator-gated and not built here — it slots in behind
/// this trait, the same Relay/Spender split mesh uses.
/// The signed reputation a settlement applies to the FIRST-ordered party — the
/// winner for [`settle`](Settle::settle), the lower-seat participant (`agentA`) for
/// [`settle_draw`](Settle::settle_draw) — with the counterparty receiving the
/// contract-applied negation, so the on-chain `recordMatchResult(+d)` /
/// `recordMatchResult(-d)` pair stays zero-sum (the core guarantees `b == -a`).
///
/// `None` defers to the contract's own FIXED `reputationDelta` — the pre-ladder
/// behaviour — so a settlement with no ranked context is byte-identical to before.
/// `Some(d)` carries the variable Elo delta [`ranked_delta`] computed from the two
/// participants' ratings: a favoured win earns less, an upset more.
type ReputationDelta = Option<i32>;

/// One seat's line in a settled multi-seat (FFA / 3+) ranked field: the agent identity
/// (the seat's roster `controller`, the harness stand-in for the on-chain address) and
/// its signed zero-sum reputation delta. The on-chain `settleField(agents[], deltas[])`
/// consumes a field as two parallel arrays in this canonical ascending-seat order, so a
/// reordering here would credit the wrong agent on-chain — the entries are built in the
/// exact order [`ranked_field_delta`] returns and never re-sorted.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FieldEntry {
    agent: String,
    delta: i32,
}

trait Settle {
    fn settle(&self, match_id: Uuid, winner: &str, reputation: ReputationDelta, replay_digest: [u8; 32]) -> Result<(), SettleError>;
    fn settle_draw(&self, match_id: Uuid, reputation: ReputationDelta, replay_digest: [u8; 32]) -> Result<(), SettleError>;
    /// Settle a multi-seat (FFA / 3+) ranked result to reputation: the zero-sum per-seat
    /// `entries` in canonical ascending-seat order, mirroring the on-chain `settleField`.
    /// No winner — placement is folded into the per-seat deltas. Reputation-only (no
    /// escrow), matching the contract slice.
    fn settle_field(&self, match_id: Uuid, entries: Vec<FieldEntry>, replay_digest: [u8; 32]) -> Result<(), SettleError>;
    fn cancel(&self, match_id: Uuid) -> Result<(), SettleError>;
}

/// One recorded resolution, mirroring the terminal state the contract would hold.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolution {
    Win { winner: String, reputation: ReputationDelta, replay_digest: [u8; 32] },
    Draw { reputation: ReputationDelta, replay_digest: [u8; 32] },
    /// A multi-seat (FFA / 3+) field: the zero-sum per-seat lines in canonical
    /// ascending-seat order, the harness mirror of the on-chain `settleField`.
    Field { entries: Vec<FieldEntry>, replay_digest: [u8; 32] },
    Cancelled,
}

/// In-process [`Settle`] for tests and the `--settle-dev-mock` path. Never touches
/// a chain: it records each resolution and models the contract's per-`match_id`
/// fence — a second resolution of ANY kind returns [`SettleError::AlreadyResolved`],
/// the same logic the on-chain `Status.Open` check applies.
///
/// The map is in-memory, so this guards a retry WITHIN one run, not across a
/// process crash: the authoritative, crash-durable double-settle guard is the
/// on-chain fence (a real submitter that crashed mid-settle re-submits and the
/// chain rejects it). The mock also does NOT re-implement the contract's other
/// input checks (winner-is-a-participant, distinct agents) — those stay the
/// contract's job; the mock models only the idempotency boundary.
#[derive(Default)]
struct MockSettler {
    resolved: RefCell<BTreeMap<Uuid, Resolution>>,
}

impl MockSettler {
    /// Apply the per-`match_id` fence, then record. Reads the map (the fence) and
    /// writes it, so a replay of any resolution is rejected exactly as the on-chain
    /// `Status.Open` check rejects a second settle/draw/cancel.
    fn record(&self, match_id: Uuid, resolution: Resolution) -> Result<(), SettleError> {
        let mut resolved = self.resolved.borrow_mut();
        if resolved.contains_key(&match_id) {
            return Err(SettleError::AlreadyResolved);
        }
        resolved.insert(match_id, resolution);
        Ok(())
    }

    fn resolution(&self, match_id: Uuid) -> Option<Resolution> {
        self.resolved.borrow().get(&match_id).cloned()
    }
}

impl Settle for MockSettler {
    fn settle(&self, match_id: Uuid, winner: &str, reputation: ReputationDelta, replay_digest: [u8; 32]) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Win { winner: winner.to_string(), reputation, replay_digest })
    }

    fn settle_draw(&self, match_id: Uuid, reputation: ReputationDelta, replay_digest: [u8; 32]) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Draw { reputation, replay_digest })
    }

    fn settle_field(&self, match_id: Uuid, entries: Vec<FieldEntry>, replay_digest: [u8; 32]) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Field { entries, replay_digest })
    }

    fn cancel(&self, match_id: Uuid) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Cancelled)
    }
}

/// The ranked-rating context a settlement needs to compute the variable reputation
/// delta: the two seats' pre-match ratings — `rating_a` for the first canonical
/// outcome seat (the lower seat id, `agentA`), `rating_b` for the second — and the
/// owner-set K-factor. Supplied by the live rating ladder; the loopback driver has
/// no ladder, so it passes `None` and the settlement defers to the contract's fixed
/// delta (byte-identical).
#[derive(Clone, Copy)]
struct RankedContext {
    rating_a: i32,
    rating_b: i32,
    k: i32,
}

/// The ranked context a MULTI-SEAT (FFA / 3+) settlement needs: every seat's pre-match
/// rating in canonical ascending-seat order — `ratings[i]` is `result.outcomes[i].seat`'s
/// rating, the same positional pairing [`ranked_field_delta`] requires — and the
/// owner-set K-factor. Supplied by the live rating ladder; an unrated agent reads as
/// [`DEFAULT_RATING`] (so a loopback field, whose agents are all unseen, settles at the
/// default), and the owner-set multi-seat K is the live driver's to pass.
#[derive(Clone)]
struct FieldContext {
    ratings: Vec<i32>,
    k: i32,
}

/// Drive one finished match through the settler: classify it, then submit the
/// matching resolution carrying the canonical `replay.digest()`. The digest is
/// taken straight from [`ReplayRecord::digest`] (not re-derived from the result's
/// hex), so the on-chain commitment is byte-identical to the recorded replay. The
/// winner identity is the winning seat's roster `controller` — the harness
/// stand-in for the on-chain agent address. Returns the chosen [`Settlement`] for
/// the caller to report. A cancel is NOT produced here: a finished match always
/// has a result; `cancel` is the pre-play abort path.
///
/// When `ranked` is supplied, the settlement carries the variable Elo reputation
/// delta [`ranked_delta`] derives from the two ratings and the outcome — the
/// winner's signed gain for a decisive result, `agentA`'s signed change for a draw
/// (negative when `agentA` was favoured). The delta is settlement metadata: it does
/// NOT touch `digest`, so the committed identity is identical with or without it.
/// `ranked == None` carries `None` (defer to the contract's fixed delta).
fn settle_match(
    settler: &impl Settle,
    result: &MatchResult,
    replay: &ReplayRecord,
    ranked: Option<RankedContext>,
) -> Result<Settlement, SettleError> {
    // settle/settleDraw are strictly 1v1; a non-pair result has no settle/settleDraw
    // form, so refuse it here (a 3+ FFA settles through settle_field_match instead)
    // rather than emit a Win/Draw the contract structurally cannot accept.
    if result.outcomes.len() != 2 {
        return Err(SettleError::NotRankedPair);
    }
    let digest = replay.digest();
    let outcome = settlement(result);
    // The zero-sum per-seat Elo delta (keyed to the canonical seat order: `.a` to the
    // first outcome seat, `.b == -.a` to the second), when a ranked context is given.
    // The 2-seat guard above means ranked_delta always yields Some here.
    let delta = ranked.map(|r| {
        ranked_delta(result, r.rating_a, r.rating_b, r.k).expect("a 2-seat result has a ranked delta")
    });
    match outcome {
        Settlement::Win { seat } => {
            let winner = replay
                .seats
                .iter()
                .find(|s| s.seat == seat)
                .map(|s| s.controller.as_str())
                .expect("the winning seat is in the roster");
            // The winner's signed reputation: `.a` if it is the first outcome seat,
            // else `.b` — always the positive side of the zero-sum split for a win.
            let reputation = delta.map(|d| if seat == result.outcomes[0].seat { d.a } else { d.b });
            settler.settle(result.match_id, winner, reputation, digest)?;
        }
        // A draw carries `agentA`'s (the first outcome seat's) signed change; the
        // contract applies its negation to `agentB`. Even ratings ⇒ 0; otherwise the
        // favoured seat moves down.
        Settlement::Draw => settler.settle_draw(result.match_id, delta.map(|d| d.a), digest)?,
    }
    Ok(outcome)
}

/// Drive a finished MULTI-SEAT (FFA / 3+) match through the settler — the sibling of
/// [`settle_match`] for a result the 1v1 `settle`/`settleDraw` cannot express. Sources
/// each seat's pre-match rating from `field` in canonical order, computes the zero-sum
/// per-seat vector [`ranked_field_delta`], pairs each delta with its seat's roster
/// `controller` (the on-chain address stand-in), and submits the whole field through
/// [`Settle::settle_field`] carrying the canonical `replay.digest()`. Returns the seat
/// count settled.
///
/// The per-seat deltas are settlement metadata: they do NOT touch `digest`, so the
/// committed identity is byte-identical with or without them — the same property the 1v1
/// reputation delta has.
///
/// Refuses anything the 1v1 path owns: fewer than 3 seats is [`SettleError::NotRankedField`]
/// (a pair settles through [`settle_match`] in the single-delta shape, never as a
/// 2-vector), and a `field` whose ratings do not align 1:1 with the seats is
/// [`SettleError::RatingsMismatch`] — refused before any emit rather than mis-paired.
fn settle_field_match(
    settler: &impl Settle,
    result: &MatchResult,
    replay: &ReplayRecord,
    field: FieldContext,
) -> Result<usize, SettleError> {
    let n = result.outcomes.len();
    if n < 3 {
        return Err(SettleError::NotRankedField);
    }
    if field.ratings.len() != n {
        return Err(SettleError::RatingsMismatch);
    }
    let digest = replay.digest();
    // n >= 3 and ratings aligned 1:1 ⇒ Some; the two guards above make this total, the
    // same way the 2-seat guard makes `ranked_delta` total in `settle_match`.
    let deltas = ranked_field_delta(result, &field.ratings, field.k)
        .expect("a >=2-seat result with aligned ratings has a field delta");
    let entries = deltas
        .into_iter()
        .map(|SeatDelta { seat, delta }| {
            // Map each canonical seat to its roster controller — the same seat→identity
            // lookup the 1v1 winner path does, per seat. The result's outcome seats are a
            // subset of the roster, so the seat is always present.
            let agent = replay
                .seats
                .iter()
                .find(|s| s.seat == seat)
                .map(|s| s.controller.clone())
                .expect("a field seat is in the roster");
            FieldEntry { agent, delta }
        })
        .collect();
    settler.settle_field(result.match_id, entries, digest)?;
    Ok(n)
}

/// The K-factor the loopback uses for ranked settlement — both the `--settle-dev-mock`
/// multi-seat field settle and the matchmaker rating ladder ([`settle_ranked_ladder`]).
/// The loopback moves only in-memory state (a [`MockSettler`] / the local ladder, never a
/// chain), so this sets the magnitude of the demonstrated deltas, not any production
/// economic knob — the live driver passes the owner-set K. 32 matches the value the
/// ranked unit tests use; sharing one constant keeps the dev-mock and ladder deltas from
/// silently diverging.
const DEV_MOCK_K: i32 = 32;

/// Stream the pre-live countdown a `starting_ticks > 0` match opens in. Each Starting
/// tick, broadcast every seat's observation (so an agent — or a spectator — sees
/// `phase == Starting` and the countdown running) then advance it with `step(&empty)`,
/// until the core flips to `Live`. Two properties this transport must hold:
///
/// - The countdown is driven by the SERVER clock, never agent input: Starting
///   observations are one-way (an agent replies only once it sees `phase == Live`, since
///   `ingest` refuses every pre-live action as `NotLive`), and `step` runs the pure
///   countdown with no intents, so no pawn moves before GO and a silent or slow agent can
///   never stall the countdown.
/// - A no-countdown match opens `Live`, so the loop never runs — byte-identical to the
///   pre-countdown pump (no Starting frame is emitted at all).
fn pump_starting(m: &mut Match, n: u8, out: &mut impl Write) {
    while m.phase() == MatchPhase::Starting {
        for seat in 0..n {
            emit(out, seat, &GatewayMsg::Observe(m.observe(seat)));
        }
        out.flush().expect("flush starting observations");
        m.step(&BTreeMap::new());
    }
}

/// Pump a formed, live match to its end: stream the pre-live countdown ([`pump_starting`]),
/// then each Live tick observe every seat, read each seat's Act (the server-authoritative
/// `ingest` forfeits a rejected action), step, and emit the terminal result to every seat.
/// The single gameplay loop both the direct and matchmade paths share — every rule still
/// lives in `arena-core`; this is transport only. Returns the canonical [`MatchResult`].
fn pump_to_end(
    m: &mut Match,
    n: u8,
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
) -> MatchResult {
    // Pre-live countdown: stream the Starting phase to the agents, then enter the Live loop
    // (which opens at tick 0). With no countdown (the default) the match opens Live and this
    // is a no-op — byte-identical to the pre-countdown pump.
    pump_starting(m, n, out);
    // The seats still in the match. A Leave drops its seat here, so the sim forfeits it
    // (absent from `intents`) every later tick AND the read loop stops expecting its line —
    // without this a departed-then-silent agent hangs the blocking read forever (or a stray
    // post-Leave line desyncs the per-tick count onto a following tick). A full active set
    // is byte-identical to the pre-Leave `0..n` (a BTreeSet iterates ascending), so a match
    // with no Leave is unchanged.
    let mut active: BTreeSet<SeatId> = (0..n).collect();
    while m.phase() == MatchPhase::Live {
        for &seat in &active {
            emit(out, seat, &GatewayMsg::Observe(m.observe(seat)));
        }
        out.flush().expect("flush observations");

        let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
        // One line per still-active seat. A Leave consumes its sender's slot for this tick
        // and departs the seat; an Act is gated on membership. A line whose seat has already
        // departed (a buggy post-Leave send) is dropped WITHOUT consuming a slot — a departed
        // seat's well-formed Act would otherwise pass `ingest` (it left, it is not dead), so
        // this membership gate, not `ingest`, is what forfeits it.
        let mut remaining = active.len();
        while remaining > 0 {
            let line = next_line(lines);
            let (seat, msg) = read_agent(&line);
            if !active.contains(&seat) {
                continue;
            }
            match msg {
                // ingest is the server-authoritative gate; a rejected action (wrong
                // tick/seat, downed, version) simply forfeits the tick.
                AgentMsg::Act(action) => {
                    if let Ok(intent) = m.ingest(seat, &action) {
                        intents.insert(seat, intent);
                    }
                }
                AgentMsg::Leave { .. } => {
                    active.remove(&seat);
                    // A Leave is a durable FORFEIT, not just a transport departure: down
                    // the seat in the sim so a 1v1 ends when the opponent is left alone
                    // (not at the max_ticks cap) and a leaver can't win on the score
                    // tiebreak. `active.remove` above stops the transport polling it; this
                    // eliminates it. Recorded into the tick's forfeits, so the golden/replay
                    // reproduces it.
                    m.forfeit(seat);
                }
                AgentMsg::Join { .. } => panic!("unexpected join during the match"),
            }
            remaining -= 1;
        }
        m.step(&intents);
    }

    finish(m, n, out)
}

/// Broadcast the terminal [`MatchResult`] to every seat and return it — the shared
/// match-end emit both the blocking ([`pump_to_end`]) and deadline-enforced
/// ([`pump_to_end_deadlined`]) live loops close on, so the two paths end a match
/// byte-identically.
fn finish(m: &Match, n: u8, out: &mut impl Write) -> MatchResult {
    let result = m.result().expect("an ended match has a result").clone();
    for seat in 0..n {
        emit(out, seat, &GatewayMsg::End(result.clone()));
    }
    out.flush().expect("flush results");
    result
}

/// Consecutive per-tick deadline misses that escalate a persistently-silent seat from a TICK
/// forfeit (it holds still but stays alive) to a durable MATCH forfeit (elimination). Small so a
/// 1v1 against a hung or dead agent ends in a few ticks instead of idling to the `max_ticks` cap,
/// but `> 1` so a single near-miss by a laggy-but-alive agent never eliminates it — only a
/// sustained silence does, and a seat that answers resets its streak. Enforced-path only; the
/// blocking golden/replay pump has no wall-clock deadline to miss.
const MISS_FORFEIT_THRESHOLD: u32 = 3;

/// Escalate the still-silent active seats when a tick's read ends without them answering (a
/// timeout, a stray-past-deadline break, or a closed stream). On a timeout (`eof == false`) each
/// silent seat's consecutive-miss streak advances and one that crosses [`MISS_FORFEIT_THRESHOLD`]
/// departs `active` — the caller then turns that departure into `m.forfeit`. On a closed stream
/// (`eof == true`) every silent seat departs at once (an immediate forfeit — a dead stream never
/// answers, so there is no streak to count). An eliminated seat's streak entry is cleared. The
/// silent set is materialised before the loop mutates `active`, so a departure never skips a peer.
fn forfeit_silent_seats(
    active: &mut BTreeSet<SeatId>,
    answered: &BTreeSet<SeatId>,
    misses: &mut BTreeMap<SeatId, u32>,
    eof: bool,
) {
    for seat in active.difference(answered).copied().collect::<Vec<_>>() {
        if eof {
            active.remove(&seat);
            misses.remove(&seat);
            continue;
        }
        let streak = misses.entry(seat).or_insert(0);
        *streak += 1;
        if *streak >= MISS_FORFEIT_THRESHOLD {
            active.remove(&seat);
            misses.remove(&seat);
        }
    }
}

/// Read one Live tick's actions from the still-`active` seats, bounded by a SHARED per-tick
/// wall-clock deadline. Every seat races the same `Instant` (computed once, here), so the
/// whole tick costs at most ~`deadline` no matter how many seats are slow — a near-miss on the
/// first seat can't extend the next seat's budget into `active.len() · deadline` (FM4). A seat
/// whose action does not arrive within the budget, or a stream that has closed
/// ([`RecvTimeoutError::Disconnected`], EOF), is simply omitted from the returned intents, so
/// `step` forfeits its tick exactly as it forfeits an absent seat — the sim already ends a match
/// whose seats keep forfeiting. `ingest` is the same server-authoritative gate the blocking path
/// applies, so a late-but-delivered action that is now stale (its tick has passed) still forfeits.
///
/// A `Leave` forfeits AND departs its seat from `active` (the enforced-path twin of
/// [`pump_to_end`]), so no later tick awaits — or is billed a per-tick timeout for — a departed
/// agent, and a stray line from an already-departed seat is dropped WITHOUT consuming a slot.
///
/// A tick forfeit alone is not durable: a hung seat misses every tick yet stays alive to the
/// `max_ticks` cap, so a 1v1 against a dead agent drags out to the cap (~an hour at 30 Hz·3600).
/// This read ESCALATES a persistent silence to elimination. `misses` (owned by the caller so it
/// survives across ticks) counts each seat's CONSECUTIVE deadline misses and is reset the moment
/// the seat answers; once a seat crosses [`MISS_FORFEIT_THRESHOLD`] it departs `active` and the
/// caller forfeits it — the same active-set-difference elimination a `Leave` takes. A closed
/// stream is an immediate departure (a dead stream never answers, so there is no streak to count):
/// every still-silent seat is dropped at once, so a mid-match EOF eliminates the dark seats and the
/// match ends the moment one team remains instead of disconnecting every later tick to the cap.
fn read_tick_deadlined(
    m: &Match,
    active: &mut BTreeSet<SeatId>,
    rx: &Receiver<String>,
    deadline: Duration,
    misses: &mut BTreeMap<SeatId, u32>,
) -> BTreeMap<SeatId, ActionIntent> {
    let tick_deadline = Instant::now() + deadline;
    let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
    // The seats that delivered a line this tick — an accepted Act, a rejected one, or a Leave.
    // A seat here is alive and talking, so it resets its consecutive-miss streak and is exempt
    // from this tick's timeout/EOF escalation; an active seat absent from this set when the
    // budget runs out is the silent one whose streak advances.
    let mut answered: BTreeSet<SeatId> = BTreeSet::new();
    let mut remaining = active.len();
    while remaining > 0 {
        let wait = tick_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(line) => {
                let (seat, msg) = read_agent(&line);
                // A line from an already-departed seat (a buggy post-Leave send) is dropped
                // without consuming a slot — the same membership gate the blocking pump applies.
                // A misbehaving departed seat could flood strays, so once the shared budget is
                // spent a stray BREAKS the read rather than draining the backlog past the
                // deadline: the tick stays ~deadline-bounded and the unread survivors forfeit
                // exactly as on a timeout (a wall-clock check, so an honest survivor whose real
                // line sits behind the flood still gets the full budget — a stray-count cap
                // would forfeit it early, this does not).
                if !active.contains(&seat) {
                    if Instant::now() >= tick_deadline {
                        // Budget spent on a stray from a departed seat: the silent survivors
                        // missed this tick exactly as on a timeout, so escalate them before
                        // breaking — else a one-stray-per-tick flood would reset the read past
                        // the deadline forever without ever advancing a co-silent seat's streak.
                        forfeit_silent_seats(active, &answered, misses, false);
                        break;
                    }
                    continue;
                }
                // The seat answered this tick: it is alive and talking, so its consecutive-miss
                // streak resets (a single near-miss never accumulates toward elimination).
                answered.insert(seat);
                misses.remove(&seat);
                match msg {
                    AgentMsg::Act(action) => {
                        if let Ok(intent) = m.ingest(seat, &action) {
                            intents.insert(seat, intent);
                        }
                    }
                    AgentMsg::Leave { .. } => {
                        active.remove(&seat);
                    }
                    AgentMsg::Join { .. } => panic!("unexpected join during the match"),
                }
                remaining -= 1;
            }
            // The tick's budget is spent: the seats that have not answered forfeit the tick. Stop
            // reading so one slow tick costs ~one deadline, not one per unread seat. Each silent
            // seat's consecutive-miss streak advances; one that crosses the threshold has gone
            // persistently dark, so it departs `active` and the caller ELIMINATES it (a hung 1v1
            // ends in a few ticks, not at the cap). A seat under the threshold only tick-forfeits.
            Err(RecvTimeoutError::Timeout) => {
                eprintln!(
                    "[deadline] tick {} read exceeded {}µs; forfeiting the unread seat(s)",
                    m.tick(),
                    deadline.as_micros()
                );
                forfeit_silent_seats(active, &answered, misses, false);
                break;
            }
            // EOF: the agent stream closed. Every still-silent seat departs `active` at once (an
            // immediate forfeit — no streak to count on a dead stream), and the caller eliminates
            // it, so the match ends the moment one team remains rather than disconnecting on every
            // later tick to the cap. An already-answered seat keeps its seat (its line landed).
            Err(RecvTimeoutError::Disconnected) => {
                forfeit_silent_seats(active, &answered, misses, true);
                break;
            }
        }
    }
    intents
}

/// Pump a live match to its end enforcing the per-tick action deadline: like
/// [`pump_to_end`] but each Live tick reads its actions through [`read_tick_deadlined`]
/// (a `recv_timeout` against the shared budget) instead of the unbounded blocking
/// [`next_line`], so a slow or hung seat forfeits on the wall clock. `rx` is fed by a
/// reader thread ([`pump_to_end_enforced`] wires the real stdin; a test feeds it
/// directly). Enforcement is opt-in ([`Args::enforce_deadline`]) precisely because this
/// path is wall-clock-driven and so non-deterministic — the golden/replay path takes
/// [`pump_to_end`] and never a timer.
fn pump_to_end_deadlined(
    m: &mut Match,
    n: u8,
    rx: &Receiver<String>,
    out: &mut impl Write,
    deadline: Duration,
) -> MatchResult {
    pump_starting(m, n, out);
    let mut active: BTreeSet<SeatId> = (0..n).collect();
    // Per-seat consecutive deadline misses, kept across ticks so a persistent silence escalates
    // to elimination (see read_tick_deadlined). A seat that answers clears its entry; a match
    // where every seat answers on time keeps this empty and byte-identical to the pre-escalation pump.
    let mut misses: BTreeMap<SeatId, u32> = BTreeMap::new();
    while m.phase() == MatchPhase::Live {
        for &seat in &active {
            emit(out, seat, &GatewayMsg::Observe(m.observe(seat)));
        }
        out.flush().expect("flush observations");
        let active_before: BTreeSet<SeatId> = active.clone();
        let intents = read_tick_deadlined(m, &mut active, rx, deadline, &mut misses);
        // Each seat that departed `active` inside the read — a Leave, a threshold-crossing silent
        // seat, or an EOF-dropped one — is forfeited so the sim ELIMINATES it (the enforced-path
        // twin of pump_to_end's inline forfeit). A single sub-threshold miss keeps its seat, so a
        // slow-but-connected agent is not eliminated here (see read_tick_deadlined).
        for &seat in active_before.difference(&active) {
            m.forfeit(seat);
        }
        m.step(&intents);
    }
    finish(m, n, out)
}

/// Feed each stdin line to `tx` until EOF, a read error, or the receiver drops (the match
/// ended). Runs on its own thread because a blocking line read has no timeout in std — the
/// pump reads from the channel with a per-tick deadline instead. Re-locks `io::stdin()`
/// itself (the caller has released the handshake's lock; the process-global buffered reader
/// keeps any bytes it had already buffered, so no line is lost across the handoff), which is
/// why the whole [`std::io::StdinLock`] — a `!Send` guard — never has to cross the thread
/// boundary.
fn feed_stdin_lines(tx: &mpsc::Sender<String>) {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(l) => {
                if tx.send(l).is_err() {
                    break; // the pump dropped the receiver: the match is over
                }
            }
            Err(_) => break, // a read error ends the feed; the pump then sees Disconnected
        }
    }
}

/// Wire the real stdin into [`pump_to_end_deadlined`]: spawn the reader thread that feeds
/// the channel, then pump. The reader is DETACHED, never joined — a thread blocked on a
/// silent-but-open stdin would otherwise hang match teardown; when this returns the
/// receiver drops, and the reader exits on its next send (or dies with the process).
fn pump_to_end_enforced(m: &mut Match, n: u8, out: &mut impl Write, deadline: Duration) -> MatchResult {
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || feed_stdin_lines(&tx));
    pump_to_end_deadlined(m, n, &rx, out, deadline)
}

/// Drive the live match through whichever read discipline the flags select, returning the
/// terminal [`MatchResult`] for the shared settle path. `deadline == None` (the default,
/// and any run without `--enforce-deadline` or with a `0`-µs budget) takes the unbounded
/// blocking [`pump_to_end`] over the handshake's `lines` iterator — byte-identical to the
/// pre-flag harness. `Some(d)` drops that iterator (releasing the stdin lock) and takes the
/// wall-clock-enforced [`pump_to_end_enforced`], whose reader thread re-locks stdin.
fn drive_pump(
    m: &mut Match,
    n: u8,
    deadline: Option<Duration>,
    mut lines: impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
) -> MatchResult {
    match deadline {
        None => pump_to_end(m, n, &mut lines, out),
        Some(d) => {
            drop(lines);
            pump_to_end_enforced(m, n, out, d)
        }
    }
}

/// The wall-clock read budget to enforce this run, or `None` to keep the blocking read.
/// `Some(d)` only when `--enforce-deadline` is set AND the declared budget is non-zero —
/// a `0`-µs deadline has nothing to enforce (it would forfeit every seat every tick), so
/// it falls back to the blocking read rather than stranding the match.
fn enforced_deadline(args: &Args, m: &Match) -> Option<Duration> {
    let micros = m.rules().action_deadline_micros;
    (args.enforce_deadline && micros > 0).then(|| Duration::from_micros(micros as u64))
}

/// Settle a finished match through the optional mock settler. Overlays any
/// handshake-recovered ranked identities onto the roster first — `recovered` is the
/// direct path's verified `(seat, address)` pairs, and EMPTY for a matchmade match,
/// whose formed roster already carries each verified address as its seat controller.
/// A 1v1 (or degenerate) result settles through [`settle_match`] (deferring to the
/// contract's fixed reputation delta); a 3+ field through [`settle_field_match`].
fn settle_finished(
    settler: &Option<MockSettler>,
    result: &MatchResult,
    m: Match,
    recovered: &[(SeatId, String)],
) {
    let Some(s) = settler else { return };
    let match_id = m.match_id();
    let mut replay = m.into_replay();
    seat_recovered_identities(&mut replay.seats, recovered);
    // Loopback agents are unrated, so each reads as DEFAULT_RATING — exactly what the
    // live ladder returns for an unseen agent. A 1v1 then defers to the contract's
    // fixed delta (None, byte-identical to pre-ladder); a 3+ field, which has no
    // fixed-delta form, settles its zero-sum placement vector. The live driver passes
    // real ladder ratings and the owner-set K in place of these.
    let seats = result.outcomes.len();
    let report = if seats > 2 {
        let field = FieldContext { ratings: vec![DEFAULT_RATING; seats], k: DEV_MOCK_K };
        settle_field_match(s, result, &replay, field).map(|n| format!("field of {n} seats"))
    } else {
        settle_match(s, result, &replay, None).map(|o| format!("{o:?}"))
    };
    match report {
        Ok(desc) => eprintln!("[settle-dev-mock] {match_id} settled as {desc}: {:?}", s.resolution(match_id)),
        Err(e) => eprintln!("[settle-dev-mock] {match_id} settle failed: {e:?}"),
    }
}

/// Settle a matchmade match's terminal `result` into the matchmaker's rating ladder.
/// [`Matchmaker::build`] registered every Agent-mode match in its pending-ranked
/// registry at formation, so the result must move the ladder AND consume that
/// registration — else it leaks until the eviction cap reaps it. The arm is chosen by
/// outcome seat count, mirroring [`settle_match`] vs [`settle_field_match`]: a 1v1 via
/// [`Matchmaker::apply_ranked_result`], a 3+/team field via
/// [`Matchmaker::apply_ranked_field_result`] (pushing a 3+ result through the 1v1 arm
/// is a silent no-op that leaks the registration — FM1). A casual / human / Mixed match
/// was never registered, so the apply is a clean no-op (`None`); likewise a replayed
/// result whose registration the first apply already consumed, so a retry or duplicate
/// End never moves the ladder twice (FM2). The K is the shared loopback `DEV_MOCK_K`
/// (FM3). Each settled seat's post-match rating + signed delta is emitted as ONE
/// structured `[ladder]` JSON line — `{"match_id","seats":[{"seat","rating","delta"}]}`,
/// the rating resolved through the roster's seat→`controller` map — so the Python SDK
/// can parse a machine-readable frame (never the old human-formatted delta line) to
/// surface an A2A author's ladder standing. A no-op (casual / human / Mixed / replay)
/// emits nothing and is not an error; the emission has no wire effect on the match.
fn settle_ranked_ladder(mm: &Matchmaker<SignatureVerifier>, result: &MatchResult, seats: &[SeatInfo]) {
    // Fold both settle arms into one (seat, delta) list: the 1v1 delta lands `.a` on the
    // first outcome seat and `.b` on the second (canonical order); the field carries its
    // own seats. A no-op arm (never registered / already settled) bails before any emit.
    let moved: Vec<(SeatId, i32)> = if result.outcomes.len() == 2 {
        match mm.apply_ranked_result(result, DEV_MOCK_K) {
            Some(d) => vec![(result.outcomes[0].seat, d.a), (result.outcomes[1].seat, d.b)],
            None => return,
        }
    } else {
        match mm.apply_ranked_field_result(result, DEV_MOCK_K) {
            Some(deltas) => deltas.iter().map(|d| (d.seat, d.delta)).collect(),
            None => return,
        }
    };
    // Pair each settled seat to its controller's POST-settle ladder rating (the apply
    // above already wrote it), keyed by seat for the SDK. An out-of-roster seat can't
    // occur — the apply validated every outcome seat against the roster — so the lookup
    // is total; DEFAULT_RATING is an inert fallback that never fires here.
    let entries: Vec<serde_json::Value> = moved
        .into_iter()
        .map(|(seat, delta)| {
            let rating = seats
                .iter()
                .find(|s| s.seat == seat)
                .and_then(|s| mm.rating(&s.controller))
                .unwrap_or(DEFAULT_RATING);
            serde_json::json!({ "seat": seat, "rating": rating, "delta": delta })
        })
        .collect();
    let line = serde_json::json!({ "match_id": result.match_id.to_string(), "seats": entries });
    eprintln!("[ladder] {line}");
}

/// The match parameters a `--mode` run forms under — the direct path's config (30 Hz,
/// the same square bounds, free-for-all teams, the empty arena) mirrored so a matchmade
/// match plays like a hand-seated one. `seats_per_match == n` makes the match form
/// exactly when the last seat joins, consuming the whole queue, so its roster is in seat
/// (submission) order and the transport's envelope seat stays the match seat.
fn matchmaker_params(n: u8, max_ticks: u64, arena: &'static str) -> MatchParams {
    MatchParams { seats_per_match: n, max_ticks, arena, ..MatchParams::default() }
}

/// Why a `--ladder-file` could not be trusted. A MISSING or empty file is NOT an error
/// (it is the legal "start fresh" path — [`read_ladder_file`] returns `Ok(None)`); these
/// are the cases where a file EXISTS with content the harness refuses to misread, so it
/// aborts loudly rather than silently resetting accumulated standings to `DEFAULT_RATING`.
#[derive(Debug)]
enum LadderFileError {
    Read(io::Error),
    Parse(serde_json::Error),
    Restore(SnapshotError),
}

impl std::fmt::Display for LadderFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LadderFileError::Read(e) => write!(f, "unreadable ladder file: {e}"),
            LadderFileError::Parse(e) => write!(f, "malformed ladder file: {e}"),
            LadderFileError::Restore(e) => write!(f, "{e}"),
        }
    }
}

/// Read a persisted ladder snapshot from `path`. A MISSING or empty (all-whitespace)
/// file is the ONLY legal "start fresh" signal and returns `Ok(None)` — a fresh ladder
/// is byte-identical to a run with no `--ladder-file`. A present, non-empty file that is
/// not valid [`LadderSnapshot`] JSON is a loud `Err`, never a silent fresh start (which
/// would erase real standings); the schema-version check lives in
/// [`Matchmaker::from_snapshot`], reported here as [`LadderFileError::Restore`].
fn read_ladder_file(path: &Path) -> Result<Option<LadderSnapshot>, LadderFileError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(LadderFileError::Read(e)),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(&bytes).map(Some).map_err(LadderFileError::Parse)
}

/// The sibling temp path a ladder write stages to before its atomic rename: same
/// directory as `path` (so the rename stays on one filesystem and so is atomic) and
/// process-unique (so a concurrent run, or a leftover temp from a crashed one, can't
/// collide on it).
fn ladder_tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(|| OsString::from("ladder"), |n| n.to_os_string());
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

/// Persist `snapshot` to `path` durably: serialize as JSON to a sibling temp file, then
/// atomic-rename it over `path`. A crash mid-write leaves the TEMP (not `path`) partial,
/// so the previous good snapshot is never truncated in place — an interrupted persist
/// loses the new write, never corrupts the old one.
fn write_ladder(path: &Path, snapshot: &LadderSnapshot) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(snapshot).expect("a LadderSnapshot always serializes");
    let tmp = ladder_tmp_path(path);
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

/// Refuse to start a run whose `--ladder-file` can't be trusted: a corrupt or
/// stale-schema ladder is reported and the process exits non-zero, NEVER silently reset
/// to `DEFAULT_RATING` (which would erase real standings under a transient read glitch
/// or a forgotten schema bump).
fn abort_ladder(path: &Path, err: &LadderFileError) -> ! {
    eprintln!("[ladder] refusing to start from {}: {err}", path.display());
    std::process::exit(1);
}

/// Construct the matchmaker for a `--mode` run, seeding its rating ladder from
/// `--ladder-file` when one is given and present so standings accumulate across runs. No
/// flag, or a missing / empty file, starts a fresh `DEFAULT_RATING` ladder — byte-identical
/// to the pre-persistence harness. A present but corrupt or wrong-schema file aborts the
/// run via [`abort_ladder`] rather than silently resetting standings.
fn build_matchmaker(args: &Args, n: u8) -> Matchmaker<SignatureVerifier> {
    // Carry the same Rules the direct path forms under (rules_from), so a matchmade match
    // plays under exactly the tuning a hand-seated one does — this is what threads
    // --perception-memory through the --mode/ranked path the matchmaker owns.
    let params = MatchParams { rules: rules_from(args), ..matchmaker_params(n, args.max_ticks, args.arena) };
    let registry = ranked_registry_from(args);
    let base = match &args.ladder_file {
        None => Matchmaker::new(SignatureVerifier, params),
        Some(path) => match read_ladder_file(path) {
            Ok(None) => Matchmaker::new(SignatureVerifier, params),
            Ok(Some(snapshot)) => Matchmaker::from_snapshot(SignatureVerifier, params, snapshot)
                .unwrap_or_else(|e| abort_ladder(path, &LadderFileError::Restore(e))),
            Err(e) => abort_ladder(path, &e),
        },
    };
    // The matchmaker scopes registration to ranked (Agent) seats itself, so passing the
    // registry unconditionally is safe — a Mixed/Human run simply never consults it.
    base.with_ranked_registry(registry)
}

/// The ranked-registration eligibility set from `--registered`, or `None` (unenforced —
/// possession-only) when no address is listed. Each address is trimmed on parse and empties
/// rejected, so an unset `$VAR` can't silently produce an enforce-but-match-nobody registry.
fn ranked_registry_from(args: &Args) -> Option<RegistrySnapshot> {
    (!args.registered.is_empty()).then(|| RegistrySnapshot::from_addresses(&args.registered))
}

/// Map a seat's Join (its claimed `agent_id` + `signature_hex`) to a matchmaker
/// [`JoinRequest`] for `mode`. The arena-01 Join carries no controller kind, so it is
/// inferred from the mode and whether a signature is present:
/// - `human`: a token-less seat is a human; a SIGNED join is an agent presenting a
///   ranked claim into a human-only match — built as a ranked agent so the matchmaker
///   refuses it `WrongKindForMode`.
/// - `agent`: every seat is an agent — ranked when signed, casual when token-less (and
///   a casual seat is refused `Unauthenticated`, since Agent mode is ranked-only).
/// - `mixed`: a seat listed in `human_seats` is a human; any other is an agent — ranked
///   when signed, casual cross-play when token-less.
fn join_request_for(
    mode: MatchMode,
    seat: SeatId,
    human_seats: &[SeatId],
    agent_id: &str,
    signature_hex: &str,
) -> JoinRequest {
    let is_human = match mode {
        MatchMode::Human => signature_hex.is_empty(),
        MatchMode::Agent => false,
        MatchMode::Mixed => human_seats.contains(&seat),
    };
    if is_human {
        JoinRequest::human(agent_id)
    } else if signature_hex.is_empty() {
        JoinRequest::casual_agent(agent_id)
    } else {
        JoinRequest::ranked_agent(agent_id, signature_hex)
    }
}

/// Emit a Reject for `seat` and terminate: a handshake refusal (version, wrong kind for
/// the mode, or an unauthenticated ranked claim) voids the opened match as a cancel
/// (refund, no result committed — exactly `MatchSettlement.cancelMatch`), then exits,
/// mirroring the direct path's reject arms. The match never forms.
fn reject_and_exit(
    out: &mut impl Write,
    settler: &Option<MockSettler>,
    match_id: Uuid,
    seat: SeatId,
    reason: String,
    cause: &str,
) -> ! {
    emit(out, seat, &GatewayMsg::Reject { reason });
    out.flush().expect("flush reject");
    if let Some(s) = settler {
        eprintln!("[settle-dev-mock] {match_id} cancel ({cause}): {:?}", s.cancel(match_id));
    }
    std::process::exit(1);
}

/// Form the match through the `arena-match` [`Matchmaker`] under `mode`, instead of
/// seating a fixed roster. Issues a per-seat challenge, then COLLECTS every Join before
/// replying: the matchmaker forms the match only on the last seat, so — unlike the
/// direct path — no Welcome can be sent until every seat is in (a driver must send all
/// Joins before blocking on its Welcome). Each seat is then routed through
/// [`Matchmaker::join`] in seat order; because a match that consumes the whole queue is
/// rostered in submission (FIFO) order, the formed seat i is transport seat i, so the
/// multiplexed envelope seat stays the match seat.
///
/// The nonce handed to the matchmaker is exactly the challenge issued to that seat — what
/// the agent signed over — NOT the formed match's id, which the matchmaker mints only
/// after admission. A version mismatch, a wrong-kind-for-mode join, or an unauthenticated
/// ranked claim emits a Reject (+ cancel settle) and exits; the match never forms.
/// Returns the [`Matchmaker`] alongside the formed [`Match`] (whose roster already
/// credits each verified ranked identity) after emitting Welcome+Start to every seat —
/// the matchmaker outlives the pump so the terminal result can settle into its ladder
/// (it registered an Agent match in `pending_ranked` at formation).
fn handshake_matchmade(
    args: &Args,
    mode: MatchMode,
    n: u8,
    settler: &Option<MockSettler>,
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
) -> (Matchmaker<SignatureVerifier>, Match) {
    let mm = build_matchmaker(args, n);

    for seat in 0..n {
        emit(out, seat, &GatewayMsg::Challenge { nonce: nonce_for(args.match_id, seat) });
    }
    out.flush().expect("flush challenges");

    // Collect every Join first — the matchmaker forms on the LAST seat, so no Welcome can
    // be issued until all are in. Version is the protocol gate checked here; kind +
    // identity are the matchmaker's, checked as each seat is routed below.
    let mut joins: BTreeMap<SeatId, (String, String)> = BTreeMap::new();
    for _ in 0..n {
        let line = next_line(lines);
        let (seat, msg) = read_agent(&line);
        let AgentMsg::Join { protocol_version, agent_id, signature_hex } = msg else {
            panic!("expected a join during the handshake");
        };
        if check_version(protocol_version).is_err() {
            reject_and_exit(
                out,
                settler,
                args.match_id,
                seat,
                format!("protocol version mismatch: ours={PROTOCOL_VERSION}, theirs={protocol_version}"),
                "handshake version mismatch",
            );
        }
        joins.insert(seat, (agent_id, signature_hex));
    }
    assert_eq!(joins.len(), usize::from(n), "expected exactly one join per seat during the handshake");

    // Route each seat through the matchmaker IN SEAT ORDER so the formed roster's seats
    // line up with the transport. The match consumes the whole queue, so it forms on the
    // last seat; earlier seats queue.
    let mut formed: Option<Match> = None;
    for seat in 0..n {
        let (agent_id, signature_hex) = &joins[&seat];
        let req = join_request_for(mode, seat, &args.human_seats, agent_id, signature_hex);
        match mm.join(mode, nonce_for(args.match_id, seat).as_bytes(), req) {
            Ok(JoinOutcome::Queued) => {}
            Ok(outcome) => formed = outcome.into_formed(),
            Err(e) => {
                reject_and_exit(out, settler, args.match_id, seat, format!("join rejected: {e}"), "join rejected")
            }
        }
    }
    let m = formed.expect("the last seat forms the match (a Mixed match needs at least one --human-seats)");

    for seat in 0..n {
        emit(
            out,
            seat,
            &GatewayMsg::Welcome { protocol_version: PROTOCOL_VERSION, match_id: m.match_id(), seat },
        );
        emit(
            out,
            seat,
            &GatewayMsg::Start {
                match_id: m.match_id(),
                config: m.config(),
                blockers: m.blockers().to_vec(),
                pickup_points: m.pickup_spawns().iter().map(|p| p.position).collect(),
            },
        );
    }
    out.flush().expect("flush welcome+start");
    (mm, m)
}

/// The combat [`Rules`] both seating paths form under, derived from the harness flags so
/// a matchmade (`--mode`) match and a hand-seated direct match play under the SAME tuning.
/// The matchmaker carries it via [`MatchParams::rules`] ([`build_matchmaker`]); the direct
/// path passes it straight to [`Match::new_with_pickups`] ([`build_direct_match`]). Every
/// `Rules` determinant is now flag-dialable, and each knob defaults to its [`Rules::default`]
/// value, so a no-flag run forms a `Rules` byte-identical to [`Rules::default`] (and the
/// pre-knob harness). The explicit construction — no `..Rules::default()` spread — means a new
/// `Rules` field added upstream fails this build until it too is threaded through a flag.
fn rules_from(args: &Args) -> Rules {
    Rules {
        perception_memory_ticks: args.perception_memory,
        fov_octant_spread: args.fov,
        aim_mode: args.aim_mode,
        friendly_fire: args.friendly_fire,
        gravity: args.gravity,
        starting_ticks: args.starting_ticks,
        weapon_mode: args.weapon_mode,
        vertical_hit_tolerance: args.vertical_hit_tolerance,
        fall_damage: args.fall_damage,
        knockback_velocity: args.knockback_velocity,
        wall_slide: args.wall_slide,
        fall_damage_threshold: args.fall_damage_threshold,
        knockback_horizontal: args.knockback_horizontal,
        dash_cooldown: args.dash_cooldown,
        pawn_radius: args.pawn_radius,
        pawn_height: args.pawn_height,
        max_shield: args.max_shield,
        start_health: args.start_health,
        damage: args.damage,
        fire_cooldown: args.fire_cooldown,
        mag_size: args.mag_size,
        max_speed: args.max_speed,
        perception_range: args.perception_range,
        weapon_range: args.weapon_range,
        hit_radius: args.hit_radius,
        melee_cooldown: args.melee_cooldown,
        melee_damage: args.melee_damage,
        melee_range: args.melee_range,
        projectile_speed: args.projectile_speed,
        action_deadline_micros: args.action_deadline_micros,
        pickup_radius: args.pickup_radius,
        pickup_respawn_cooldown: args.pickup_respawn_cooldown,
        spawn_jitter: args.spawn_jitter,
        spawn_radius: args.spawn_radius,
    }
}

/// Build the direct-path (no `--mode`) match: a fixed `agent-{i}` free-for-all roster
/// under the configured arena geometry. The matchmade path forms its own match through
/// the [`Matchmaker`] ([`build_matchmaker`]); this is the hand-seated twin.
///
/// Both paths resolve geometry through [`arena_map`], so `--map` reaches the direct
/// path too. The default empty arena (`args.arena == ""`) yields empty blockers +
/// pickups, which is exactly what [`Match::new`] produces (it is `new_with_pickups`
/// with no pickups) — so a no-flag run is byte-identical to the pre-map harness.
fn build_direct_match(args: &Args, n: u8) -> Match {
    let roster: Vec<SeatInfo> = (0..n)
        .map(|i| SeatInfo { seat: i, team: u16::from(i), controller: format!("agent-{i}") })
        .collect();
    let config = MatchConfig {
        tick_hz: 30,
        max_ticks: args.max_ticks,
        bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
        seats: n,
    };
    let map = arena_map(args.arena);
    let rules = rules_from(args);
    Match::new_with_pickups(
        args.match_id,
        config,
        rules,
        roster,
        map.blockers,
        map.pickups,
        args.seed,
    )
}

fn main() {
    let args = parse_args();
    let n = args.seats;

    // The off-chain settlement seam: a finished match (or a pre-play abort) maps to
    // a MatchSettlement resolution through this settler. Mock-only and opt-in here;
    // the live Base submitter is operator-gated.
    let settler = args.settle_dev_mock.then(MockSettler::default);

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // --mode routes formation through the arena-match Matchmaker (mode-gated,
    // authenticated). Its formed roster already credits each verified ranked identity,
    // so settlement overlays no recovered ids.
    if let Some(mode) = args.mode {
        let (mm, mut m) = handshake_matchmade(&args, mode, n, &settler, &mut lines, &mut out);
        let deadline = enforced_deadline(&args, &m);
        let result = drive_pump(&mut m, n, deadline, lines, &mut out);
        // Settle the ladder while the roster is still alive (it maps seat→controller for
        // the rating readout); `settle_finished` then consumes the match.
        settle_ranked_ladder(&mm, &result, m.seats());
        settle_finished(&settler, &result, m, &[]);
        if let Some(path) = &args.ladder_file {
            // Persist the POST-settle ladder (the settle above moved it) so the next run
            // resumes these standings; atomic temp-then-rename keeps a crash mid-write
            // from corrupting the prior good snapshot.
            write_ladder(path, &mm.snapshot()).unwrap_or_else(|e| {
                eprintln!("[ladder] failed to persist to {}: {e}", path.display());
                std::process::exit(1);
            });
        }
        return;
    }

    // Direct-seating path (no --mode): seat a fixed agent-{i} roster, byte-identical to
    // the pre-matchmaker harness.
    let mut m = build_direct_match(&args, n);

    for seat in 0..n {
        emit(&mut out, seat, &GatewayMsg::Challenge { nonce: nonce_for(m.match_id(), seat) });
    }
    out.flush().expect("flush challenges");

    // Reply to each Join the moment it arrives — a seat's Welcome+Start must not
    // wait on another seat's Join, or a client that connects sequentially and
    // blocks on its own Welcome would deadlock against a harness waiting for the
    // next Join.
    let mut recovered: Vec<(SeatId, String)> = Vec::new();
    for _ in 0..n {
        let line = next_line(&mut lines);
        let (seat, msg) = read_agent(&line);
        let AgentMsg::Join { protocol_version, agent_id, signature_hex } = msg else {
            panic!("expected a join during the handshake");
        };
        if check_version(protocol_version).is_err() {
            emit(
                &mut out,
                seat,
                &GatewayMsg::Reject {
                    reason: format!(
                        "protocol version mismatch: ours={PROTOCOL_VERSION}, theirs={protocol_version}"
                    ),
                },
            );
            out.flush().expect("flush reject");
            if let Some(s) = &settler {
                // An opened match that can never be played voids as a cancel —
                // refund, no result committed — exactly MatchSettlement.cancelMatch.
                eprintln!(
                    "[settle-dev-mock] {} cancel (handshake version mismatch): {:?}",
                    m.match_id(),
                    s.cancel(m.match_id())
                );
            }
            std::process::exit(1);
        }
        // The agent signed join_digest over THIS seat's challenge nonce; recover it
        // and refuse a presented-but-invalid ranked proof, mirroring the version arm.
        // An empty signature is an unranked seat and admits unchanged.
        let nonce = nonce_for(m.match_id(), seat);
        match admit_join(&agent_id, nonce.as_bytes(), &signature_hex) {
            Err(e) => {
                emit(
                    &mut out,
                    seat,
                    &GatewayMsg::Reject { reason: format!("join signature rejected: {e:?}") },
                );
                out.flush().expect("flush reject");
                if let Some(s) = &settler {
                    // A presented-but-invalid ranked proof voids the opened match like a
                    // version mismatch — refund, no result committed — exactly cancelMatch.
                    eprintln!(
                        "[settle-dev-mock] {} cancel (join signature rejected): {:?}",
                        m.match_id(),
                        s.cancel(m.match_id())
                    );
                }
                std::process::exit(1);
            }
            // A verified ranked seat is seated under the address it proved (the recovered
            // signer verify_join_signature accepted as the claim); unranked keeps its label.
            Ok(Some(address)) => recovered.push((seat, address)),
            Ok(None) => {}
        }
        emit(
            &mut out,
            seat,
            &GatewayMsg::Welcome { protocol_version: PROTOCOL_VERSION, match_id: m.match_id(), seat },
        );
        emit(
            &mut out,
            seat,
            &GatewayMsg::Start {
                match_id: m.match_id(),
                config: m.config(),
                blockers: m.blockers().to_vec(),
                pickup_points: m.pickup_spawns().iter().map(|p| p.position).collect(),
            },
        );
        out.flush().expect("flush welcome+start");
    }

    let deadline = enforced_deadline(&args, &m);
    let result = drive_pump(&mut m, n, deadline, lines, &mut out);
    settle_finished(&settler, &result, m, &recovered);
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_proto::{address_from_verifying_key, join_digest, Action, ActionButtons, SeatOutcome};
    use k256::ecdsa::{RecoveryId, Signature, SigningKey};

    const MID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn id() -> Uuid {
        Uuid::parse_str(MID).unwrap()
    }

    fn roster(n: u8) -> Vec<SeatInfo> {
        (0..n).map(|i| SeatInfo { seat: i, team: u16::from(i), controller: format!("agent-{i}") }).collect()
    }

    fn replay_for(seats: Vec<SeatInfo>) -> ReplayRecord {
        ReplayRecord {
            protocol_version: PROTOCOL_VERSION,
            match_id: id(),
            seed: 0,
            seats,
            blockers: Vec::new(),
            pickups: Vec::new(),
            rules_commit: Vec::new(),
            config: MatchConfig::default(),
            ticks: Vec::new(),
        }
    }

    fn outcome(seat: SeatId, placement: u16, score: i32, alive: bool) -> SeatOutcome {
        SeatOutcome { seat, team: u16::from(seat), placement, score, alive_at_end: alive, forfeited: false }
    }

    fn result_for(outcomes: Vec<SeatOutcome>) -> MatchResult {
        MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: id(),
            final_tick: 1,
            outcomes,
            replay_hash: "00".repeat(32),
        }
    }

    #[test]
    fn win_settles_once_with_the_winner_identity_and_core_digest() {
        // Seat 1 — NOT the first outcome — wins, so this also pins that the driver
        // resolves the placement-1 seat's controller rather than `outcomes[0]`.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 2, 1, false), outcome(1, 1, 5, true)]);
        let settler = MockSettler::default();

        let chosen = settle_match(&settler, &result, &replay, None).expect("settles");
        assert_eq!(chosen, Settlement::Win { seat: 1 });
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-1".into(), reputation: None, replay_digest: replay.digest() }),
            "no ranked context ⇒ reputation None (defer to the contract's fixed delta), winner identity + core digest",
        );
    }

    #[test]
    fn retry_is_a_no_op_and_never_double_settles() {
        // FM1: a crash/retry after the match ends must not settle twice. The second
        // submit hits the per-matchId fence (AlreadyResolved) and leaves the
        // recorded resolution untouched.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]);
        let settler = MockSettler::default();

        settle_match(&settler, &result, &replay, None).expect("first settles");
        let first = settler.resolution(id());
        assert!(matches!(
            settle_match(&settler, &result, &replay, None),
            Err(SettleError::AlreadyResolved)
        ));
        assert_eq!(settler.resolution(id()), first, "the retry changes nothing");
    }

    #[test]
    fn a_tie_settles_as_a_draw_not_a_win() {
        // FM3: a draw must take settleDraw, never settle(winner). Both seats share
        // placement 1, so a win-only mapping would wrongly record a Win.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 4, true), outcome(1, 1, 4, true)]);
        let settler = MockSettler::default();

        let chosen = settle_match(&settler, &result, &replay, None).expect("settles");
        assert_eq!(chosen, Settlement::Draw);
        assert_eq!(settler.resolution(id()), Some(Resolution::Draw { reputation: None, replay_digest: replay.digest() }));
    }

    #[test]
    fn cancel_records_a_cancel_and_fences_a_later_settle() {
        // FM3 cancel mapping + FM1 fence across kinds: a cancelled match is
        // Cancelled (no winner, no committed digest) and can never then be settled.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]);
        let settler = MockSettler::default();

        settler.cancel(id()).expect("cancels");
        assert_eq!(settler.resolution(id()), Some(Resolution::Cancelled));
        assert!(matches!(settler.cancel(id()), Err(SettleError::AlreadyResolved)), "retry cancel is a no-op");
        assert!(
            matches!(settle_match(&settler, &result, &replay, None), Err(SettleError::AlreadyResolved)),
            "a cancelled match can never be settled",
        );
        assert_eq!(settler.resolution(id()), Some(Resolution::Cancelled), "still cancelled");
    }

    #[test]
    fn a_non_pair_match_is_not_settleable() {
        // settle_match is the 1v1 seam: MatchSettlement's settle/settleDraw take exactly
        // two agents, so a 3-seat FFA (and a single/empty result) is refused here rather
        // than emitted as an unsettleable Win/Draw — settle_field_match is what settles a
        // 3+ field. Nothing is recorded.
        let replay = replay_for(roster(3));
        let result =
            result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 3, true), outcome(2, 3, 1, false)]);
        let settler = MockSettler::default();

        assert!(matches!(
            settle_match(&settler, &result, &replay, None),
            Err(SettleError::NotRankedPair)
        ));
        assert_eq!(settler.resolution(id()), None, "a non-pair match records nothing");
    }

    #[test]
    fn committed_digest_equals_the_core_replay_digest() {
        // FM2: the digest committed toward settlement is byte-identical to
        // arena-core's canonical ReplayRecord.digest() of the played match — and to
        // the hex in the published MatchResult — so the on-chain commitment verifies
        // against the recorded replay. Driven by a really-simulated match, not a
        // fixture.
        let config = MatchConfig {
            tick_hz: 30,
            max_ticks: 2,
            bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
            seats: 2,
        };
        let mut m = Match::new(id(), config, Rules::default(), roster(2), Vec::new(), 0);
        while m.phase() == MatchPhase::Live {
            m.step(&BTreeMap::new());
        }
        let result = m.result().expect("ended").clone();
        let replay = m.into_replay();
        let settler = MockSettler::default();

        settle_match(&settler, &result, &replay, None).expect("settles");
        let committed = match settler.resolution(id()).expect("resolved") {
            Resolution::Win { replay_digest, .. }
            | Resolution::Draw { replay_digest, .. }
            | Resolution::Field { replay_digest, .. } => replay_digest,
            Resolution::Cancelled => panic!("a played match is never a cancel"),
        };
        assert_eq!(committed, replay.digest(), "commits the exact core digest");
        assert_eq!(hex::encode(committed), result.replay_hash, "matches the published result hash");
    }

    fn ranked(rating_a: i32, rating_b: i32, k: i32) -> RankedContext {
        RankedContext { rating_a, rating_b, k }
    }

    #[test]
    fn a_ranked_win_carries_the_winners_exact_zero_sum_core_delta() {
        // FM2: with a ranked context the settle carries EXACTLY the core ranked_delta's
        // winner side; the loser's −d is the contract-applied negation the core
        // guarantees (b == −a). Seat 1 (NOT the first outcome) wins from an even match,
        // so this also pins the winner→delta mapping picks `.b`, not `.a`.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 2, 1, false), outcome(1, 1, 5, true)]);
        let k = 32;
        let core = ranked_delta(&result, DEFAULT_RATING, DEFAULT_RATING, k).unwrap();
        assert_eq!(core.a, -core.b, "the core delta is zero-sum");
        assert!(core.b > 0, "seat 1 is the winner, so its side (.b) is the positive gain");

        let settler = MockSettler::default();
        settle_match(&settler, &result, &replay, Some(ranked(DEFAULT_RATING, DEFAULT_RATING, k))).unwrap();
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-1".into(), reputation: Some(core.b), replay_digest: replay.digest() }),
            "the settle carries the winning seat's exact core delta",
        );
    }

    #[test]
    fn a_favoured_win_carries_less_reputation_than_an_upset_win() {
        // The variable delta tracks the rating gap: the SAME win (seat 0) earns the
        // winner LESS when favoured than when the underdog. Pinned against the core,
        // and the carried value matches the favoured computation verbatim.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]); // seat 0 (agentA) wins
        let k = 32;
        let favoured = ranked_delta(&result, 1900, 1500, k).unwrap().a; // agentA favoured
        let upset = ranked_delta(&result, 1300, 1500, k).unwrap().a; // agentA underdog
        assert!(favoured > 0 && upset > 0, "a win always gains");
        assert!(favoured < upset, "the favourite earns less for the same win ({favoured} < {upset})");

        let settler = MockSettler::default();
        settle_match(&settler, &result, &replay, Some(ranked(1900, 1500, k))).unwrap();
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-0".into(), reputation: Some(favoured), replay_digest: replay.digest() }),
        );
    }

    #[test]
    fn a_draw_carries_agent_a_signed_core_delta() {
        // FM4 (draw): a draw between UNEQUAL ratings moves the favoured agentA (seat 0)
        // DOWN — settle_draw carries agentA's negative core delta (the contract negates
        // it onto agentB). Even ratings ⇒ a zero draw delta.
        let replay = replay_for(roster(2));
        let tie = result_for(vec![outcome(0, 1, 4, true), outcome(1, 1, 4, true)]);
        let k = 32;
        let core = ranked_delta(&tie, 1800, 1500, k).unwrap();
        assert!(core.a < 0, "a draw moves the favoured agentA down");

        let favoured = MockSettler::default();
        settle_match(&favoured, &tie, &replay, Some(ranked(1800, 1500, k))).unwrap();
        assert_eq!(
            favoured.resolution(id()),
            Some(Resolution::Draw { reputation: Some(core.a), replay_digest: replay.digest() }),
        );

        let even = MockSettler::default();
        settle_match(&even, &tie, &replay, Some(ranked(DEFAULT_RATING, DEFAULT_RATING, k))).unwrap();
        assert_eq!(
            even.resolution(id()),
            Some(Resolution::Draw { reputation: Some(0), replay_digest: replay.digest() }),
            "an even draw carries a zero delta",
        );
    }

    #[test]
    fn the_reputation_delta_never_perturbs_the_committed_digest() {
        // FM3: the delta is settlement metadata, not a digest input — the committed
        // digest is identical with a fixed (None) or a variable (Some) reputation, and
        // both equal the canonical core ReplayRecord.digest().
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]);
        let dig = |s: &MockSettler| match s.resolution(id()).expect("resolved") {
            Resolution::Win { replay_digest, .. }
            | Resolution::Draw { replay_digest, .. }
            | Resolution::Field { replay_digest, .. } => replay_digest,
            Resolution::Cancelled => unreachable!("a settled win is never a cancel"),
        };
        let fixed = MockSettler::default();
        settle_match(&fixed, &result, &replay, None).unwrap();
        let variable = MockSettler::default();
        settle_match(&variable, &result, &replay, Some(ranked(1700, 1400, 32))).unwrap();
        assert_eq!(dig(&fixed), dig(&variable), "the reputation delta does not change the committed digest");
        assert_eq!(dig(&variable), replay.digest(), "still the canonical core digest");
    }

    fn field(ratings: Vec<i32>, k: i32) -> FieldContext {
        FieldContext { ratings, k }
    }

    fn three_seat_result() -> MatchResult {
        // Strict 1/2/3 finish so every pairwise game is decisive (no ties to flatten the
        // per-seat deltas).
        result_for(vec![outcome(0, 1, 9, true), outcome(1, 2, 5, true), outcome(2, 3, 1, false)])
    }

    #[test]
    fn a_field_settle_maps_each_canonical_seat_to_its_controller_and_core_delta() {
        // FM1: a 3-seat result emits the zero-sum per-seat vector in canonical
        // ascending-seat order, each delta paired to ITS seat's controller. Distinct
        // ratings + a strict placement make the three deltas distinct, so a seat→agent
        // swap or a dropped seat is observable; the carried deltas equal arena-core's
        // ranked_field_delta verbatim.
        let replay = replay_for(roster(3));
        let result = three_seat_result();
        let k = 32;
        let ratings = vec![1500, 1400, 1600];
        let core = ranked_field_delta(&result, &ratings, k).expect("3-seat field has deltas");
        assert_eq!(core.iter().map(|d| i64::from(d.delta)).sum::<i64>(), 0, "the field is zero-sum");
        let ds: Vec<i32> = core.iter().map(|d| d.delta).collect();
        assert!(ds[0] != ds[1] && ds[1] != ds[2] && ds[0] != ds[2], "distinct deltas make a swap observable: {ds:?}");

        let settler = MockSettler::default();
        let n = settle_field_match(&settler, &result, &replay, field(ratings, k)).expect("settles");
        assert_eq!(n, 3, "all three seats settled");

        let expected: Vec<FieldEntry> = core
            .iter()
            .map(|d| FieldEntry { agent: format!("agent-{}", d.seat), delta: d.delta })
            .collect();
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Field { entries: expected, replay_digest: replay.digest() }),
            "each canonical seat maps to its controller + its exact core delta",
        );
    }

    #[test]
    fn a_field_settle_keys_the_controller_by_seat_id_not_roster_position() {
        // FM1 (position-vs-seat-id): the controller is keyed by SEAT ID, not by the
        // delta's position in the roster Vec — a roster stored out of seat order would
        // make a positional lookup credit the wrong agent. The identity-roster test above
        // can't see this (there seat == position == name), so build a roster whose Vec
        // order (seats 2, 0, 1) is decoupled from seat id and pin that each seat's delta
        // still lands on the controller whose seat matches.
        let seats = vec![
            SeatInfo { seat: 2, team: 2, controller: "carol".into() },
            SeatInfo { seat: 0, team: 0, controller: "alice".into() },
            SeatInfo { seat: 1, team: 1, controller: "bob".into() },
        ];
        let replay = replay_for(seats);
        let result = three_seat_result(); // outcomes sorted ascending: seats 0, 1, 2
        let k = 32;
        let ratings = vec![1500, 1400, 1600];
        let core = ranked_field_delta(&result, &ratings, k).unwrap();

        let settler = MockSettler::default();
        settle_field_match(&settler, &result, &replay, field(ratings, k)).expect("settles");

        let expected = vec![
            FieldEntry { agent: "alice".into(), delta: core[0].delta }, // seat 0
            FieldEntry { agent: "bob".into(), delta: core[1].delta },   // seat 1
            FieldEntry { agent: "carol".into(), delta: core[2].delta }, // seat 2
        ];
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Field { entries: expected, replay_digest: replay.digest() }),
            "each seat's delta lands on the controller whose seat matches, not roster position",
        );
    }

    #[test]
    fn the_field_seam_refuses_a_pair_so_n2_keeps_the_single_delta_shape() {
        // FM2: a 2-seat result must never be emitted as a 2-vector. The field seam refuses
        // a pair (NotRankedField), so the ONLY n=2 settle path is settle_match's single
        // winner/agentA delta — the live 1v1 path is unchanged.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]);

        let field_settler = MockSettler::default();
        assert!(matches!(
            settle_field_match(&field_settler, &result, &replay, field(vec![1500, 1500], 32)),
            Err(SettleError::NotRankedField),
        ));
        assert_eq!(field_settler.resolution(id()), None, "a pair records no field resolution");

        let pair_settler = MockSettler::default();
        let k = 32;
        let core = ranked_delta(&result, 1500, 1500, k).unwrap();
        settle_match(&pair_settler, &result, &replay, Some(ranked(1500, 1500, k))).unwrap();
        assert_eq!(
            pair_settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-0".into(), reputation: Some(core.a), replay_digest: replay.digest() }),
            "n=2 settles as the single winner delta, never a 2-vector",
        );
    }

    #[test]
    fn the_field_deltas_never_perturb_the_committed_digest() {
        // FM3: the per-seat deltas are settlement metadata — the committed digest is
        // identical across two settles of the SAME result with DIFFERENT ratings (hence
        // different deltas), and equals the canonical core ReplayRecord.digest().
        let replay = replay_for(roster(3));
        let result = three_seat_result();
        let ent = |s: &MockSettler| match s.resolution(id()).expect("resolved") {
            Resolution::Field { entries, replay_digest } => (entries, replay_digest),
            _ => unreachable!("a field settle records a Field"),
        };
        let a = MockSettler::default();
        settle_field_match(&a, &result, &replay, field(vec![1500, 1500, 1500], 32)).unwrap();
        let b = MockSettler::default();
        settle_field_match(&b, &result, &replay, field(vec![1900, 1300, 1700], 64)).unwrap();
        let (ea, da) = ent(&a);
        let (eb, db) = ent(&b);
        assert_ne!(ea, eb, "the two rating sets really produce different deltas");
        assert_eq!(da, db, "the field deltas do not change the committed digest");
        assert_eq!(da, replay.digest(), "still the canonical core digest");
    }

    #[test]
    fn a_field_settle_is_fenced_against_a_replay() {
        // The shared per-matchId fence: a second field settle of the same matchId is
        // AlreadyResolved and the first recorded vector is untouched — the off-chain
        // mirror of the on-chain Status fence (one settlement per matchId).
        let replay = replay_for(roster(3));
        let result = three_seat_result();
        let settler = MockSettler::default();
        settle_field_match(&settler, &result, &replay, field(vec![1500, 1500, 1500], 32)).expect("first settles");
        let first = settler.resolution(id());
        assert!(matches!(
            settle_field_match(&settler, &result, &replay, field(vec![1900, 1300, 1700], 64)),
            Err(SettleError::AlreadyResolved),
        ));
        assert_eq!(settler.resolution(id()), first, "the replay changes nothing");
    }

    #[test]
    fn a_field_settle_refuses_a_misaligned_or_subfield_result() {
        // RatingsMismatch: ratings must align 1:1 with the seats, else the positional
        // seat→rating pairing is wrong — refused before any emit. NotRankedField: fewer
        // than 3 seats is the 1v1 path's job, so a single/empty result is refused here.
        let replay3 = replay_for(roster(3));
        let result3 = three_seat_result();
        let settler = MockSettler::default();
        assert!(matches!(
            settle_field_match(&settler, &result3, &replay3, field(vec![1500, 1500], 32)),
            Err(SettleError::RatingsMismatch),
        ));
        assert_eq!(settler.resolution(id()), None, "a misaligned vector records nothing");

        let single = result_for(vec![outcome(0, 1, 1, true)]);
        assert!(matches!(
            settle_field_match(&settler, &single, &replay_for(roster(1)), field(vec![1500], 32)),
            Err(SettleError::NotRankedField),
        ));
        let empty = result_for(vec![]);
        assert!(matches!(
            settle_field_match(&settler, &empty, &replay_for(roster(0)), field(vec![], 32)),
            Err(SettleError::NotRankedField),
        ));
        assert_eq!(settler.resolution(id()), None, "nothing recorded across the refusals");
    }

    fn join_key() -> SigningKey {
        let bytes =
            hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    fn other_join_key() -> SigningKey {
        let bytes =
            hex::decode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    fn third_join_key() -> SigningKey {
        let bytes =
            hex::decode("7777777777777777777777777777777777777777777777777777777777777777").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    /// Sign join_digest exactly as the agent SDK does — `[r||s||v]` hex, low-S, raw
    /// recovery id — so these tests exercise admit_join over a real agent proof.
    fn sign_join_proof(sk: &SigningKey, agent_id: &str, nonce: &[u8]) -> String {
        let digest = join_digest(PROTOCOL_VERSION, agent_id, nonce);
        let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(&digest).unwrap();
        let mut raw = sig.to_bytes().to_vec();
        raw.push(recid.to_byte());
        hex::encode(raw)
    }

    #[test]
    fn admit_join_admits_a_valid_ranked_signature() {
        // The agent signs join_digest over its seat's challenge nonce with its session
        // key; admit_join recovers the signer and accepts the identity it claims.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let nonce = nonce_for(id(), 0);
        let sig = sign_join_proof(&sk, &addr, nonce.as_bytes());
        // Admitted AND the recovered identity is the claimed address — the seat is later
        // seated under it, not the roster label.
        assert_eq!(admit_join(&addr, nonce.as_bytes(), &sig), Ok(Some(addr.clone())));
    }

    #[test]
    fn admit_join_admits_an_empty_signature_as_unranked() {
        // The baseline's default: no signature is an unranked seat, admitted with no
        // proof — the loopback is not ranked-only, so unranked play is untouched.
        let nonce = nonce_for(id(), 0);
        // Admitted with NO recovered identity — an unranked seat keeps its roster label.
        assert_eq!(admit_join("0xanyone", nonce.as_bytes(), ""), Ok(None));
    }

    #[test]
    fn admit_join_rejects_a_forged_claim_to_another_identity() {
        // A seat signs with its OWN key but claims a different agent_id (here the other
        // key's address): the recovered signer is not the claim, so the seat is refused
        // — a forger cannot present an identity whose key it does not hold.
        let sk = join_key();
        let nonce = nonce_for(id(), 0);
        let claimed = address_from_verifying_key(other_join_key().verifying_key());
        let sig = sign_join_proof(&sk, &claimed, nonce.as_bytes());
        assert_eq!(admit_join(&claimed, nonce.as_bytes(), &sig), Err(JoinVerifyError::AddressMismatch));
    }

    #[test]
    fn admit_join_rejects_a_signature_replayed_under_a_different_nonce() {
        // A Join captured on one seat's connection (nonce A) is worthless on another
        // (nonce B): the nonce is folded into the digest, so the signature recovers a
        // different address against B and is refused — cross-connection replay closed.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let sig = sign_join_proof(&sk, &addr, nonce_for(id(), 0).as_bytes());
        let other_nonce = nonce_for(id(), 1);
        assert_eq!(
            admit_join(&addr, other_nonce.as_bytes(), &sig),
            Err(JoinVerifyError::AddressMismatch)
        );
    }

    #[test]
    fn admit_join_rejects_a_malformed_signature() {
        // A PRESENTED but non-hex / wrong-length signature is a bad encoding, not waved
        // through: a ranked claim with a junk proof is refused, never silently seated.
        let nonce = nonce_for(id(), 0);
        assert_eq!(
            admit_join("0xclaim", nonce.as_bytes(), "not-hex"),
            Err(JoinVerifyError::BadSignatureEncoding)
        );
        assert_eq!(
            admit_join("0xclaim", nonce.as_bytes(), "00"),
            Err(JoinVerifyError::BadSignatureEncoding)
        );
    }

    #[test]
    fn seat_recovered_identities_seats_ranked_addresses_and_keeps_unranked_labels() {
        // FM1 + FM4: a Mixed roster — seat 0 is verified ranked, seat 1 is unranked. The
        // ranked seat adopts the address it proved during the handshake; the unranked seat
        // (no entry in the recovered set) keeps its agent-1 roster label.
        let mut seats = roster(2);
        let addr = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23".to_string();
        seat_recovered_identities(&mut seats, &[(0, addr.clone())]);
        assert_eq!(seats[0].controller, addr, "the ranked seat adopts the recovered address");
        assert_eq!(seats[1].controller, "agent-1", "the unranked seat keeps its roster label");
    }

    #[test]
    fn seat_recovered_identities_changes_only_the_label_not_seat_or_team() {
        // FM3: the identity overlay touches the controller LABEL only — seat and team stay
        // index-driven, so seat order, team assignment, and the match's reproducibility are
        // untouched even when every seat is ranked.
        let mut seats = roster(2);
        seat_recovered_identities(&mut seats, &[(0, "0xaaa".into()), (1, "0xbbb".into())]);
        assert_eq!(
            (seats[0].seat, seats[0].team, seats[1].seat, seats[1].team),
            (0, 0, 1, 1),
            "seat and team are untouched by the identity overlay",
        );
    }

    #[test]
    fn seat_recovered_identities_with_no_ranked_seats_keeps_every_roster_label() {
        // FM1: an all-unranked match (empty recovered set) is byte-identical to before —
        // every seat keeps agent-{i}, so unranked play is never perturbed by the overlay.
        let mut seats = roster(3);
        let before = seats.clone();
        seat_recovered_identities(&mut seats, &[]);
        assert_eq!(seats, before, "no recovered identities ⇒ the roster is unchanged");
    }

    #[test]
    fn a_ranked_win_settles_the_recovered_address_not_the_roster_label() {
        // FM2 end to end: seat 1 wins a 1v1 after being seated under the address it proved
        // ranked, so settle_match must credit THAT address, not agent-1. Were the recovered
        // identity not overlaid onto the seat settle_match reads, the winner would settle as
        // the agent-1 roster label — so crediting the real address is the discriminating proof.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let mut replay = replay_for(roster(2));
        seat_recovered_identities(&mut replay.seats, &[(1, addr.clone())]);
        let result = result_for(vec![outcome(0, 2, 1, false), outcome(1, 1, 5, true)]);
        let settler = MockSettler::default();

        settle_match(&settler, &result, &replay, None).expect("settles");
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: addr, reputation: None, replay_digest: replay.digest() }),
            "the verified ranked identity is credited, not the agent-1 roster label",
        );
    }

    // ===== arena-match Matchmaker entry (--mode) =====

    use arena_match::JoinError;
    use arena_proto::ControllerKind;

    fn mode_args(seats: u8, mode: MatchMode, human_seats: Vec<SeatId>) -> Args {
        Args {
            match_id: id(),
            seed: 0,
            seats,
            max_ticks: 4,
            settle_dev_mock: false,
            mode: Some(mode),
            human_seats,
            ladder_file: None,
            registered: Vec::new(),
            arena: "",
            perception_memory: 0,
            fov: 4,
            aim_mode: AimMode::Octant,
            friendly_fire: false,
            gravity: 0,
            starting_ticks: 0,
            weapon_mode: WeaponMode::Hitscan,
            vertical_hit_tolerance: 0,
            fall_damage: 0,
            knockback_velocity: 0,
            wall_slide: false,
            fall_damage_threshold: 0,
            knockback_horizontal: 0,
            dash_cooldown: 0,
            pawn_radius: 0,
            pawn_height: 0,
            max_shield: 0,
            start_health: Rules::default().start_health,
            damage: Rules::default().damage,
            fire_cooldown: Rules::default().fire_cooldown,
            mag_size: Rules::default().mag_size,
            max_speed: Rules::default().max_speed,
            perception_range: Rules::default().perception_range,
            weapon_range: Rules::default().weapon_range,
            hit_radius: Rules::default().hit_radius,
            melee_cooldown: Rules::default().melee_cooldown,
            melee_damage: Rules::default().melee_damage,
            melee_range: Rules::default().melee_range,
            projectile_speed: Rules::default().projectile_speed,
            action_deadline_micros: Rules::default().action_deadline_micros,
            enforce_deadline: false,
            pickup_radius: Rules::default().pickup_radius,
            pickup_respawn_cooldown: Rules::default().pickup_respawn_cooldown,
            spawn_jitter: Rules::default().spawn_jitter,
            spawn_radius: Rules::default().spawn_radius,
        }
    }

    /// A transport envelope carrying one seat's Join, exactly as the matchmade
    /// handshake reads it off the pipe.
    fn join_line(seat: SeatId, agent_id: &str, signature_hex: &str) -> String {
        let frame = serde_json::to_value(AgentMsg::Join {
            protocol_version: PROTOCOL_VERSION,
            agent_id: agent_id.to_string(),
            signature_hex: signature_hex.to_string(),
        })
        .unwrap();
        serde_json::json!({ "seat": seat, "frame": frame }).to_string()
    }

    fn mm2() -> Matchmaker<SignatureVerifier> {
        Matchmaker::new(SignatureVerifier, matchmaker_params(2, 4, ""))
    }

    #[test]
    fn parse_mode_maps_each_name() {
        assert_eq!(parse_mode("human"), MatchMode::Human);
        assert_eq!(parse_mode("agent"), MatchMode::Agent);
        assert_eq!(parse_mode("mixed"), MatchMode::Mixed);
    }

    #[test]
    fn matchmaker_params_mirror_the_direct_seating_config() {
        // A matchmade match must play like a hand-seated one: same tick rate, bounds,
        // free-for-all teams, empty arena — and seats_per_match == n so it forms exactly
        // when the last seat joins (consuming the whole queue, rostered in seat order).
        let p = matchmaker_params(3, 1234, "");
        assert_eq!(p.seats_per_match, 3);
        assert_eq!(p.max_ticks, 1234);
        assert_eq!(p.tick_hz, 30);
        assert_eq!(p.team_size, 1, "free-for-all, like the direct roster's team == seat");
        assert_eq!(p.bounds, Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE });
        assert_eq!(p.arena, "", "the empty arena, like the direct path's no-pickups match");
        assert_eq!(
            matchmaker_params(3, 1234, "reference").arena,
            "reference",
            "a named arena threads through to the matchmaker, so --map reaches the matchmade path"
        );
    }

    // ===== --map arena selection =====

    fn direct_args(seats: u8, arena: &'static str, perception_memory: u16) -> Args {
        Args {
            match_id: id(),
            seed: 0,
            seats,
            max_ticks: 4,
            settle_dev_mock: false,
            mode: None,
            human_seats: vec![],
            ladder_file: None,
            registered: Vec::new(),
            arena,
            perception_memory,
            fov: 4,
            aim_mode: AimMode::Octant,
            friendly_fire: false,
            gravity: 0,
            starting_ticks: 0,
            weapon_mode: WeaponMode::Hitscan,
            vertical_hit_tolerance: 0,
            fall_damage: 0,
            knockback_velocity: 0,
            wall_slide: false,
            fall_damage_threshold: 0,
            knockback_horizontal: 0,
            dash_cooldown: 0,
            pawn_radius: 0,
            pawn_height: 0,
            max_shield: 0,
            start_health: Rules::default().start_health,
            damage: Rules::default().damage,
            fire_cooldown: Rules::default().fire_cooldown,
            mag_size: Rules::default().mag_size,
            max_speed: Rules::default().max_speed,
            perception_range: Rules::default().perception_range,
            weapon_range: Rules::default().weapon_range,
            hit_radius: Rules::default().hit_radius,
            melee_cooldown: Rules::default().melee_cooldown,
            melee_damage: Rules::default().melee_damage,
            melee_range: Rules::default().melee_range,
            projectile_speed: Rules::default().projectile_speed,
            action_deadline_micros: Rules::default().action_deadline_micros,
            enforce_deadline: false,
            pickup_radius: Rules::default().pickup_radius,
            pickup_respawn_cooldown: Rules::default().pickup_respawn_cooldown,
            spawn_jitter: Rules::default().spawn_jitter,
            spawn_radius: Rules::default().spawn_radius,
        }
    }

    /// The first [`GatewayMsg::Start`] decoded out of the harness's emitted stdout
    /// envelopes — what an agent actually receives, proving the geometry crosses the wire
    /// and isn't merely held on the in-memory `Match`.
    fn first_start(stdout: &str) -> GatewayMsg {
        stdout
            .lines()
            .find_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).ok()?;
                let msg: GatewayMsg = serde_json::from_value(v.get("frame")?.clone()).ok()?;
                matches!(msg, GatewayMsg::Start { .. }).then_some(msg)
            })
            .expect("a Start frame is emitted")
    }

    #[test]
    fn parse_arena_resolves_a_known_key() {
        assert_eq!(parse_arena("reference"), "reference");
    }

    #[test]
    #[should_panic(expected = "unknown arena")]
    fn parse_arena_rejects_an_unknown_key() {
        // FM2: a typo must abort loudly, NOT degrade through arena_map to the empty arena
        // (which would silently play no-cover instead of the map the operator asked for).
        parse_arena("does-not-exist");
    }

    #[test]
    fn parse_registered_trims_and_collects_each_occurrence() {
        // Each --registered is one address; whitespace (e.g. a trailing newline from a
        // shell/file read) is trimmed so it still matches the canonical recovered address.
        let parsed = parse_args_from(
            ["--registered", "0xAbC", "--registered", " 0xdef\n", "--seats", "2"].into_iter().map(String::from),
        );
        assert_eq!(parsed.registered, vec!["0xAbC".to_string(), "0xdef".to_string()]);
    }

    #[test]
    #[should_panic(expected = "non-empty agent address")]
    fn parse_registered_rejects_an_empty_value() {
        // FM: an unset $VAR expands to one empty arg; enforcing an empty registry would
        // reject every ranked seat with a misleading 'unauthenticated'. Fail loud at parse.
        parse_args_from(["--registered", "", "--seats", "2"].into_iter().map(String::from));
    }

    #[test]
    fn parse_fov_accepts_the_whole_domain() {
        // 0 (facing octant alone) through 4 (full circle) are the sim's valid spreads.
        assert_eq!((0..=4).map(|s| parse_fov(&s.to_string())).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    #[should_panic(expected = "0..=4")]
    fn parse_fov_rejects_an_out_of_range_spread() {
        // FM1: a spread >4 must abort loudly, NOT saturate to the full circle in the sim
        // (which would silently play omnidirectional instead of the cone asked for).
        parse_fov("7");
    }

    #[test]
    fn parse_aim_mode_maps_each_name() {
        assert_eq!(parse_aim_mode("octant"), AimMode::Octant);
        assert_eq!(parse_aim_mode("fine"), AimMode::Fine);
    }

    #[test]
    #[should_panic(expected = "octant|fine")]
    fn parse_aim_mode_rejects_an_unknown_name() {
        // FM2: an unrecognized aim name must abort loudly, NOT default to Octant — aim_mode
        // is a hit-resolution determinant, so a silent default would mis-resolve combat and
        // commit a replay that disagrees with what the operator asked for.
        parse_aim_mode("coarse");
    }

    #[test]
    fn parse_gravity_maps_a_non_negative_magnitude() {
        assert_eq!(parse_gravity("0"), 0);
        assert_eq!(parse_gravity("500"), 500);
        assert_eq!(parse_gravity(&i32::MAX.to_string()), i32::MAX, "the whole non-negative i32 range is valid");
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_gravity_rejects_a_negative() {
        // FM2: a negative behaves as 0 (off) in core (vertical physics is gated on gravity > 0),
        // so forwarding it would silently run a 2D match the operator did not ask for — reject it
        // loudly at the CLI instead. Parsing the value as u32 fails a leading '-'.
        parse_gravity("-500");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_gravity_rejects_an_overflow() {
        // FM2: a magnitude past i32::MAX must abort, NOT wrap the integer fall integration into a
        // negative (which would then read as off). 3_000_000_000 fits a u32 but not an i32, so the
        // i32::try_from narrowing catches it.
        parse_gravity("3000000000");
    }

    #[test]
    fn parse_vertical_hit_tolerance_maps_a_non_negative_band() {
        assert_eq!(parse_vertical_hit_tolerance("0"), 0);
        assert_eq!(parse_vertical_hit_tolerance("100"), 100);
        assert_eq!(
            parse_vertical_hit_tolerance(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid band"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_vertical_hit_tolerance_rejects_a_negative() {
        // A negative band would invert the |shooter_z - target_z| <= tol comparison in core, so a
        // forwarded negative would silently flip hit resolution — reject it loudly at the CLI. The
        // u32 parse fails the leading '-'.
        parse_vertical_hit_tolerance("-100");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_vertical_hit_tolerance_rejects_an_overflow() {
        // A band past i32::MAX must abort, NOT wrap into a negative (which would then invert the
        // comparison). 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_vertical_hit_tolerance("3000000000");
    }

    #[test]
    fn parse_knockback_velocity_maps_a_non_negative_impulse() {
        assert_eq!(parse_knockback_velocity("0"), 0);
        assert_eq!(parse_knockback_velocity("800"), 800);
        assert_eq!(
            parse_knockback_velocity(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid impulse"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_knockback_velocity_rejects_a_negative() {
        // A negative impulse would launch the hit target DOWNWARD into the floor, so a forwarded
        // negative would silently invert the pop-up — reject it loudly at the CLI. The u32 parse
        // fails the leading '-'.
        parse_knockback_velocity("-800");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_knockback_velocity_rejects_an_overflow() {
        // An impulse past i32::MAX must abort, NOT wrap into a negative (which would then launch the
        // target downward). 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_knockback_velocity("3000000000");
    }

    #[test]
    fn parse_fall_damage_threshold_maps_a_non_negative_gate() {
        assert_eq!(parse_fall_damage_threshold("0"), 0);
        assert_eq!(parse_fall_damage_threshold("3000"), 3000);
        assert_eq!(
            parse_fall_damage_threshold(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid gate"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_fall_damage_threshold_rejects_a_negative() {
        // Core compares `impact > threshold`, so a negative gate would make EVERY landing (impact > 0)
        // wound — the inverse of raising the bar. Reject it loudly at the CLI; the u32 parse fails the
        // leading '-'.
        parse_fall_damage_threshold("-3000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_fall_damage_threshold_rejects_an_overflow() {
        // A gate past i32::MAX must abort, NOT wrap into a negative (which would then wound every
        // landing). 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_fall_damage_threshold("3000000000");
    }

    #[test]
    fn parse_knockback_horizontal_maps_a_non_negative_shove() {
        assert_eq!(parse_knockback_horizontal("0"), 0);
        assert_eq!(parse_knockback_horizontal("200"), 200);
        assert_eq!(
            parse_knockback_horizontal(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid shove"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_knockback_horizontal_rejects_a_negative() {
        // Core gates the shove on `> 0`, so a negative is INERT (silently no shove — the operator
        // dialed a pull and got nothing). Reject it loudly at the CLI; the u32 parse fails the
        // leading '-'.
        parse_knockback_horizontal("-200");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_knockback_horizontal_rejects_an_overflow() {
        // A shove past i32::MAX must abort, NOT wrap into a negative (which core would then read as
        // off). 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_knockback_horizontal("3000000000");
    }

    #[test]
    fn parse_pawn_radius_maps_a_non_negative_radius() {
        assert_eq!(parse_pawn_radius("0"), 0);
        assert_eq!(parse_pawn_radius("750"), 750);
        assert_eq!(
            parse_pawn_radius(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid radius"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_pawn_radius_rejects_a_negative() {
        // Core gates occupancy on `pawn_radius > 0`, so a negative is INERT (silently no pawn-vs-pawn
        // collision — the operator dialed a body and got a ghost). Reject it loudly at the CLI; the u32
        // parse fails the leading '-'.
        parse_pawn_radius("-750");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_pawn_radius_rejects_an_overflow() {
        // A radius past i32::MAX must abort, NOT wrap into a negative (which core would then read as
        // off). 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_pawn_radius("3000000000");
    }

    #[test]
    fn parse_pawn_height_maps_a_non_negative_band() {
        assert_eq!(parse_pawn_height("0"), 0);
        assert_eq!(parse_pawn_height("1800"), 1800);
        assert_eq!(
            parse_pawn_height(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid band"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_pawn_height_rejects_a_negative() {
        // Core gates the z-band on `pawn_height > 0`, so a negative is INERT (silently planar occupancy).
        // Reject it loudly at the CLI; the u32 parse fails the leading '-'.
        parse_pawn_height("-1800");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_pawn_height_rejects_an_overflow() {
        // A band past i32::MAX must abort, NOT wrap into a negative (which core would then read as
        // planar). 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_pawn_height("3000000000");
    }

    #[test]
    fn parse_weapon_mode_maps_each_name() {
        assert_eq!(parse_weapon_mode("hitscan"), WeaponMode::Hitscan);
        assert_eq!(parse_weapon_mode("projectile"), WeaponMode::Projectile);
        assert_eq!(parse_weapon_mode("melee"), WeaponMode::Melee);
    }

    #[test]
    #[should_panic(expected = "hitscan|projectile|melee")]
    fn parse_weapon_mode_rejects_an_unknown_name() {
        // FM2: an unrecognized weapon name must abort loudly, NOT default to Hitscan — weapon_mode
        // decides how a fire resolves (instant beam / traveling projectile / melee cleave), so a
        // silent default would run a different weapon than asked and commit a disagreeing replay.
        parse_weapon_mode("railgun");
    }

    #[test]
    fn direct_match_default_arena_is_empty() {
        // FM1: no --map (arena == "") yields empty geometry — exactly Match::new's
        // no-blockers/no-pickups match, so the no-flag run stays byte-identical.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        assert!(m.blockers().is_empty(), "the default arena has no cover");
        assert!(m.pickup_spawns().is_empty(), "the default arena has no items");
        assert_eq!(m.rules().perception_memory_ticks, 0, "no --perception-memory: memory off");
    }

    #[test]
    fn direct_match_named_arena_loads_cover_and_pickups() {
        // FM3/FM4: --map reference reaches the DIRECT path — the formed match carries the
        // reference arena's occluder + two health pickups.
        let m = build_direct_match(&direct_args(2, "reference", 0), 2);
        assert!(!m.blockers().is_empty(), "the reference arena has a vision occluder");
        assert_eq!(m.pickup_spawns().len(), 2, "the reference arena has two health pickups");
    }

    #[test]
    fn direct_match_threads_the_perception_memory_window_into_rules() {
        // FM1/FM3: --perception-memory reaches the sim's Rules (the seat memory.rs reads),
        // so the knob actually turns the core feature on; default 0 stays off. The memory
        // BEHAVIOR (a lost enemy surfaces in_line_of_sight=false) is arena-core's own test;
        // here we pin the wiring deterministically via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().perception_memory_ticks,
            0,
            "the default window is off (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&direct_args(2, "reference", 45), 2).rules().perception_memory_ticks,
            45,
            "--perception-memory 45 threads into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_the_perception_window_into_a_matchmade_match() {
        // The frontier this slice closes: --perception-memory now reaches the --mode path.
        // build_matchmaker carries the window via MatchParams.rules, so a match the
        // MATCHMAKER forms runs under it — not the Rules::default() the matchmaker hardcoded
        // before. Proven by forming a 2-seat match through the built matchmaker (Human seats
        // are token-less, so no signing) and reading rules() back, the accessor the
        // direct-path twin above uses — so matchmade and hand-seated agree on the window.
        let mm = build_matchmaker(&direct_args(2, "", 45), 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().perception_memory_ticks,
            45,
            "the matchmaker forms under the --perception-memory window (matchmade == hand-seated)"
        );

        // No flag still forms memory-off — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(off.rules().perception_memory_ticks, 0, "no --perception-memory: the matchmaker forms memory-off");
    }

    #[test]
    fn direct_match_threads_the_fov_cone_into_rules() {
        // FM2: --fov reaches the sim's Rules (the in_fov perception cone); default 4 = full
        // circle so a no-flag run is byte-identical (and its replay digest unchanged). The
        // cone BEHAVIOR (an out-of-cone enemy is not perceived) is arena-core's own test;
        // here we pin the wiring deterministically via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().fov_octant_spread,
            4,
            "no --fov is the full circle (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { fov: 1, ..direct_args(2, "reference", 0) }, 2).rules().fov_octant_spread,
            1,
            "--fov 1 threads the narrow cone into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_the_fov_cone_into_a_matchmade_match() {
        // FM3 (path skew): --fov must reach the --mode path too, not just the direct one.
        // build_matchmaker carries the cone via MatchParams.rules, so a MATCHMADE match
        // forms under it — proven by forming a 2-seat Human match through the built
        // matchmaker and reading rules() back, the accessor the direct twin uses (so
        // matchmade and hand-seated agree on the cone).
        let mm = build_matchmaker(&Args { fov: 1, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().fov_octant_spread,
            1,
            "the matchmaker forms under the --fov cone (matchmade == hand-seated)"
        );

        // No flag still forms full-circle — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(off.rules().fov_octant_spread, 4, "no --fov: the matchmaker forms omnidirectional");
    }

    #[test]
    fn direct_match_threads_the_aim_mode_into_rules() {
        // FM1 (default drift): no --aim-mode is Octant — the 8-way snap, byte-identical to the
        // pre-flag harness (and its replay digest). The aim BEHAVIOR (a sub-octant lead lands
        // under Fine) is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().aim_mode,
            AimMode::Octant,
            "no --aim-mode is Octant (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { aim_mode: AimMode::Fine, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .aim_mode,
            AimMode::Fine,
            "--aim-mode fine threads Fine into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_the_aim_mode_into_a_matchmade_match() {
        // FM3 (path skew): --aim-mode must reach the --mode path too, not just the direct one.
        // build_matchmaker carries the mode via MatchParams.rules, so a MATCHMADE match forms
        // under it — proven by forming a 2-seat Human match and reading rules() back (the same
        // accessor the direct twin uses, so matchmade and hand-seated agree on the aim).
        let mm = build_matchmaker(&Args { aim_mode: AimMode::Fine, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().aim_mode,
            AimMode::Fine,
            "the matchmaker forms under --aim-mode fine (matchmade == hand-seated)"
        );

        // No flag still forms Octant — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(off.rules().aim_mode, AimMode::Octant, "no --aim-mode: the matchmaker forms Octant");
    }

    #[test]
    fn direct_match_threads_friendly_fire_into_rules() {
        // FM1 (default drift): no --friendly-fire spares allies (Rules::friendly_fire == false),
        // byte-identical to the pre-flag harness (and its replay digest). The allied-damage
        // BEHAVIOR is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert!(
            !build_direct_match(&direct_args(2, "", 0), 2).rules().friendly_fire,
            "no --friendly-fire spares allies (byte-identical to the pre-flag harness)"
        );
        assert!(
            build_direct_match(&Args { friendly_fire: true, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .friendly_fire,
            "--friendly-fire threads allied damage into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_friendly_fire_into_a_matchmade_match() {
        // FM3 (path skew): --friendly-fire must reach the --mode path too, not just the direct
        // one. build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms
        // under it — proven by forming a 2-seat Human match and reading rules() back (the same
        // accessor the direct twin uses, so matchmade and hand-seated agree on allied damage).
        let mm = build_matchmaker(&Args { friendly_fire: true, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert!(
            formed.rules().friendly_fire,
            "the matchmaker forms under --friendly-fire (matchmade == hand-seated)"
        );

        // No flag still spares allies — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert!(!off.rules().friendly_fire, "no --friendly-fire: the matchmaker spares allies");
    }

    #[test]
    fn friendly_fire_flag_consumes_no_following_token() {
        // FM2 (flag-with-value confusion): --friendly-fire is a PRESENCE flag — it flips the bool
        // WITHOUT swallowing the next token. If the arm wrongly called it.next(), it would eat the
        // following --seats and that token's "3" would abort as an unknown argument. Pin that a
        // --friendly-fire IMMEDIATELY before --seats 3 parses BOTH: the flag on, seats == 3.
        let parsed = parse_args_from(["--friendly-fire", "--seats", "3"].into_iter().map(String::from));
        assert!(parsed.friendly_fire, "--friendly-fire flips the flag");
        assert_eq!(parsed.seats, 3, "--friendly-fire consumed no token, so --seats 3 still parsed");

        // Absent, the parse loop defaults it off (the parse-level twin of the threading FM1).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert!(!none.friendly_fire, "no --friendly-fire defaults off");
    }

    #[test]
    fn direct_match_threads_gravity_into_rules() {
        // FM1 (default drift): no --gravity is 0 — vertical physics off, every pawn z stays 0,
        // byte-identical to the pre-flag 2D harness (and its replay digest). The jump/z BEHAVIOR
        // is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().gravity,
            0,
            "no --gravity is 0 (vertical physics off, byte-identical to the 2D harness)"
        );
        assert_eq!(
            build_direct_match(&Args { gravity: 500, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .gravity,
            500,
            "--gravity 500 threads the magnitude into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_gravity_into_a_matchmade_match() {
        // FM3 (path skew): --gravity must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under it —
        // proven by forming a 2-seat Human match and reading rules() back (the same accessor the
        // direct twin uses, so matchmade and hand-seated agree on the gravity).
        let mm = build_matchmaker(&Args { gravity: 500, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().gravity,
            500,
            "the matchmaker forms under --gravity 500 (matchmade == hand-seated)"
        );

        // No flag still forms gravity 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(off.rules().gravity, 0, "no --gravity: the matchmaker forms vertical physics off");
    }

    #[test]
    fn direct_match_threads_starting_ticks_and_opens_in_starting() {
        // FM1 (default drift): no --starting-ticks is 0 — the match opens directly in Live at tick
        // 0, byte-identical to the pre-countdown harness. The countdown BEHAVIOR (N steps then Live)
        // is arena-core's own test; here we pin the wiring via rules() + phase().
        let plain = build_direct_match(&direct_args(2, "reference", 0), 2);
        assert_eq!(plain.rules().starting_ticks, 0, "no --starting-ticks is 0");
        assert_eq!(plain.phase(), MatchPhase::Live, "no countdown opens directly in Live");

        let counted = build_direct_match(&Args { starting_ticks: 3, ..direct_args(2, "reference", 0) }, 2);
        assert_eq!(counted.rules().starting_ticks, 3, "--starting-ticks 3 threads the count into Rules");
        assert_eq!(counted.phase(), MatchPhase::Starting, "a positive countdown opens the match in Starting");
    }

    #[test]
    fn build_matchmaker_threads_starting_ticks_into_a_matchmade_match() {
        // FM3 (path skew): --starting-ticks must reach the --mode path too. build_matchmaker carries
        // it via MatchParams.rules (the same rules_from both paths share), so a MATCHMADE match
        // forms under it.
        let mm = build_matchmaker(&Args { starting_ticks: 4, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(formed.rules().starting_ticks, 4, "the matchmaker forms under --starting-ticks 4");
    }

    #[test]
    fn starting_ticks_flag_parses_and_defaults_off() {
        // The parse-level twin of the threading test: --starting-ticks consumes its value; absent
        // it defaults to 0 (no countdown).
        let parsed =
            parse_args_from(["--starting-ticks", "5", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.starting_ticks, 5, "--starting-ticks 5 parses its value");
        assert_eq!(parsed.seats, 3, "the value was consumed, so --seats 3 still parsed");

        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(none.starting_ticks, 0, "no --starting-ticks defaults to 0");
    }

    #[test]
    fn pump_starting_streams_the_countdown_then_opens_live() {
        // A starting_ticks=N match opens in Starting; pump_starting broadcasts every seat's
        // observation (phase=Starting, tick pinned at 0) for exactly N ticks then flips to Live
        // at tick 0 — driven by the server clock (no agent input read), applying no action.
        let mut m = build_direct_match(&Args { starting_ticks: 2, ..direct_args(2, "reference", 0) }, 2);
        assert_eq!(m.phase(), MatchPhase::Starting, "the countdown match opens in Starting");
        let before = [m.observe(0).own.position, m.observe(1).own.position];

        let mut out: Vec<u8> = Vec::new();
        pump_starting(&mut m, 2, &mut out);

        // The countdown elapsed to Live at tick 0, no pawn moved pre-live (a Starting step runs
        // the pure countdown with empty intents).
        assert_eq!(m.phase(), MatchPhase::Live, "the countdown flips to Live");
        assert_eq!(m.tick(), 0, "Live opens at tick 0 — the countdown never advanced the tick");
        assert_eq!(
            [m.observe(0).own.position, m.observe(1).own.position],
            before,
            "no pawn moved during the countdown"
        );

        // Exactly N ticks × n seats Starting observations were broadcast, each phase=Starting at
        // tick 0, in seat order — a spectator/agent connected during the countdown sees it run.
        let text = String::from_utf8(out).expect("utf8 transcript");
        let frames: Vec<serde_json::Value> =
            text.lines().map(|l| serde_json::from_str(l).expect("frame json")).collect();
        assert_eq!(frames.len(), 2 * 2, "N=2 countdown ticks × 2 seats = 4 Starting observations");
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f["seat"].as_u64(), Some((i % 2) as u64), "observations cycle in seat order 0,1,0,1");
            // GatewayMsg is internally tagged (`type`), so the Observation fields sit flat on the frame.
            let obs = &f["frame"];
            assert_eq!(obs["type"], "observe", "each Starting frame is an Observe");
            assert_eq!(obs["phase"], "starting", "every broadcast frame carries phase=Starting");
            assert_eq!(obs["tick"].as_u64(), Some(0), "the countdown observations all sit at tick 0");
        }
    }

    #[test]
    fn pump_starting_is_a_no_op_without_a_countdown() {
        // The default: a starting_ticks=0 match opens Live, so pump_starting emits NOTHING and
        // steps nothing — byte-identical to the pre-countdown pump (no Starting frame at all).
        let mut m = build_direct_match(&direct_args(2, "reference", 0), 2);
        assert_eq!(m.phase(), MatchPhase::Live, "no countdown opens directly in Live");

        let mut out: Vec<u8> = Vec::new();
        pump_starting(&mut m, 2, &mut out);

        assert!(out.is_empty(), "a no-countdown match emits no Starting observation");
        assert_eq!(m.phase(), MatchPhase::Live, "the match stays Live");
        assert_eq!(m.tick(), 0, "and at tick 0 — pump_starting did not step it");
    }

    #[test]
    fn direct_match_threads_vertical_hit_tolerance_into_rules() {
        // FM1 (default drift): no --vertical-hit-tolerance is 0 — combat planar, z ignored in hit
        // resolution, byte-identical to the pre-flag harness (and its replay digest). The z-coupled
        // HIT behavior is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().vertical_hit_tolerance,
            0,
            "no --vertical-hit-tolerance is 0 (combat planar, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { vertical_hit_tolerance: 100, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .vertical_hit_tolerance,
            100,
            "--vertical-hit-tolerance 100 threads the band into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_vertical_hit_tolerance_into_a_matchmade_match() {
        // FM3 (path skew): --vertical-hit-tolerance must reach the --mode path too, not just the
        // direct one. build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms
        // under the same band a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { vertical_hit_tolerance: 100, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().vertical_hit_tolerance,
            100,
            "the matchmaker forms under --vertical-hit-tolerance 100 (matchmade == hand-seated)"
        );

        // No flag still forms tolerance 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().vertical_hit_tolerance,
            0,
            "no --vertical-hit-tolerance: the matchmaker forms combat planar"
        );
    }

    #[test]
    fn direct_match_threads_fall_damage_into_rules() {
        // FM1 (default drift): no --fall-damage is 0 — every landing safe, byte-identical to the
        // pre-flag harness (and its replay digest). The hard-landing DAMAGE behavior is arena-core's
        // own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().fall_damage,
            0,
            "no --fall-damage is 0 (every landing safe, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { fall_damage: 25, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .fall_damage,
            25,
            "--fall-damage 25 threads the magnitude into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_fall_damage_into_a_matchmade_match() {
        // FM3 (path skew): --fall-damage must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the
        // same magnitude a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { fall_damage: 25, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().fall_damage,
            25,
            "the matchmaker forms under --fall-damage 25 (matchmade == hand-seated)"
        );

        // No flag still forms fall_damage 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().fall_damage,
            0,
            "no --fall-damage: the matchmaker forms every landing safe"
        );
    }

    #[test]
    fn fall_damage_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --fall-damage pulls exactly one token and
        // parses it as the u16 magnitude, consuming no following flag. A --fall-damage 25 right
        // before --seats 3 must parse BOTH (25 hp, seats 3); absent, it defaults 0.
        let parsed =
            parse_args_from(["--fall-damage", "25", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.fall_damage, 25, "--fall-damage 25 parses the magnitude");
        assert_eq!(parsed.seats, 3, "--fall-damage consumed exactly one token, so --seats 3 parsed");

        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(none.fall_damage, 0, "no --fall-damage defaults 0 (every landing safe)");
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn fall_damage_rejects_an_overflow() {
        // FM2 (type bound): fall_damage is a u16; a value past u16::MAX must abort at the CLI, NOT
        // wrap into a small magnitude (65536 would wrap to 0 — silently safe landings the operator
        // did not ask for). The u16 parse catches it; a negative aborts the same way ('-' is not a
        // u16 digit).
        parse_args_from(["--fall-damage", "65536"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_knockback_velocity_into_rules() {
        // FM1 (default drift): no --knockback-velocity is 0 — a hit imparts no vertical impulse,
        // byte-identical to the pre-flag harness (and its replay digest). The pop-up BEHAVIOR (a hit
        // launches the survivor upward, arcing back under gravity) is arena-core's own test; here we
        // pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().knockback_velocity,
            0,
            "no --knockback-velocity is 0 (no impulse, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { knockback_velocity: 800, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .knockback_velocity,
            800,
            "--knockback-velocity 800 threads the impulse into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_knockback_velocity_into_a_matchmade_match() {
        // FM3 (path skew): --knockback-velocity must reach the --mode path too, not just the direct
        // one. build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the
        // same impulse a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { knockback_velocity: 800, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().knockback_velocity,
            800,
            "the matchmaker forms under --knockback-velocity 800 (matchmade == hand-seated)"
        );

        // No flag still forms knockback_velocity 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().knockback_velocity,
            0,
            "no --knockback-velocity: the matchmaker forms no impulse"
        );
    }

    #[test]
    fn direct_match_threads_fall_damage_threshold_into_rules() {
        // FM1 (default drift): no --fall-damage-threshold is 0 — the gate is wide open (every impact>0
        // landing takes the full fall_damage once that is on), byte-identical to the pre-flag harness
        // (and its replay digest). The soft-landing-spared BEHAVIOR is arena-core's own test; here we
        // pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().fall_damage_threshold,
            0,
            "no --fall-damage-threshold is 0 (gate wide open, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { fall_damage_threshold: 3000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .fall_damage_threshold,
            3000,
            "--fall-damage-threshold 3000 threads the gate into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_fall_damage_threshold_into_a_matchmade_match() {
        // FM3 (path skew): --fall-damage-threshold must reach the --mode path too, not just the direct
        // one. build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the
        // same gate a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { fall_damage_threshold: 3000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().fall_damage_threshold,
            3000,
            "the matchmaker forms under --fall-damage-threshold 3000 (matchmade == hand-seated)"
        );

        // No flag still forms fall_damage_threshold 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().fall_damage_threshold,
            0,
            "no --fall-damage-threshold: the matchmaker forms the gate wide open"
        );
    }

    #[test]
    fn direct_match_threads_knockback_horizontal_into_rules() {
        // FM1 (default drift): no --knockback-horizontal is 0 — a hit imparts no planar shove (the
        // target's pos is unchanged), byte-identical to the pre-flag harness (and its replay digest).
        // The shove BEHAVIOR (a hit displaces the survivor along the bearing through slide()) is
        // arena-core's own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().knockback_horizontal,
            0,
            "no --knockback-horizontal is 0 (no shove, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { knockback_horizontal: 200, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .knockback_horizontal,
            200,
            "--knockback-horizontal 200 threads the shove into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_knockback_horizontal_into_a_matchmade_match() {
        // FM3 (path skew): --knockback-horizontal must reach the --mode path too, not just the direct
        // one. build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the
        // same shove a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { knockback_horizontal: 200, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().knockback_horizontal,
            200,
            "the matchmaker forms under --knockback-horizontal 200 (matchmade == hand-seated)"
        );

        // No flag still forms knockback_horizontal 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().knockback_horizontal,
            0,
            "no --knockback-horizontal: the matchmaker forms no shove"
        );
    }

    #[test]
    fn direct_match_threads_wall_slide_into_rules() {
        // FM1 (default drift): no --wall-slide leaves wall_slide false — a grazing move dead-stops at
        // its origin, byte-identical to the pre-flag harness (and its replay digest). The slide-along
        // BEHAVIOR (a blocked diagonal projects onto the surface and slides) is arena-core's own test;
        // here we pin the wiring via the rules() accessor.
        assert!(
            !build_direct_match(&direct_args(2, "", 0), 2).rules().wall_slide,
            "no --wall-slide stops a grazing move (byte-identical to the pre-flag harness)"
        );
        assert!(
            build_direct_match(&Args { wall_slide: true, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .wall_slide,
            "--wall-slide threads slide-along-a-blocker into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_wall_slide_into_a_matchmade_match() {
        // FM3 (path skew): --wall-slide must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under it —
        // proven by forming a 2-seat Human match and reading rules() back (the same accessor the
        // direct twin uses, so matchmade and hand-seated agree on the movement rule).
        let mm = build_matchmaker(&Args { wall_slide: true, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert!(
            formed.rules().wall_slide,
            "the matchmaker forms under --wall-slide (matchmade == hand-seated)"
        );

        // No flag still dead-stops a grazing move — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert!(!off.rules().wall_slide, "no --wall-slide: the matchmaker stops a grazing move");
    }

    #[test]
    fn wall_slide_flag_consumes_no_following_token() {
        // FM2 (flag-with-value confusion): --wall-slide is a PRESENCE flag — it flips the bool WITHOUT
        // swallowing the next token. If the arm wrongly called it.next(), it would eat the following
        // --seats and that token's "3" would abort as an unknown argument. Pin that a --wall-slide
        // IMMEDIATELY before --seats 3 parses BOTH: the flag on, seats == 3.
        let parsed = parse_args_from(["--wall-slide", "--seats", "3"].into_iter().map(String::from));
        assert!(parsed.wall_slide, "--wall-slide flips the flag");
        assert_eq!(parsed.seats, 3, "--wall-slide consumed no token, so --seats 3 still parsed");

        // Absent, the parse loop defaults it off (the parse-level twin of the threading FM1).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert!(!none.wall_slide, "no --wall-slide defaults off");
    }

    #[test]
    fn dash_cooldown_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --dash-cooldown pulls exactly one token and
        // parses it as the u16 cadence, consuming no following flag. A --dash-cooldown 20 right before
        // --seats 3 must parse BOTH (20 ticks, seats 3); absent, it defaults 0 (dash off).
        let parsed =
            parse_args_from(["--dash-cooldown", "20", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.dash_cooldown, 20, "--dash-cooldown 20 parses the cadence");
        assert_eq!(parsed.seats, 3, "--dash-cooldown consumed exactly one token, so --seats 3 parsed");

        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(none.dash_cooldown, 0, "no --dash-cooldown defaults 0 (dash disabled)");
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn dash_cooldown_rejects_an_overflow() {
        // FM2 (type bound): dash_cooldown is a u16; a value past u16::MAX must abort at the CLI, NOT
        // wrap into a small cadence (65536 would wrap to 0 — a zero-cooldown dash-every-tick the
        // operator did not ask for, and 0 ALSO reads as "dash off", so the wrap is doubly wrong). The
        // u16 parse catches it; a negative aborts the same way ('-' is not a u16 digit).
        parse_args_from(["--dash-cooldown", "65536"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_dash_cooldown_into_rules() {
        // FM1 (default drift): no --dash-cooldown is 0 — the ability press is inert, byte-identical to
        // the pre-flag harness (and its replay digest). The dash BEHAVIOR (an ability press bursts the
        // pawn DASH_DISTANCE, then the cadence gates the next) is arena-core's own test; here we pin
        // the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().dash_cooldown,
            0,
            "no --dash-cooldown is 0 (dash off, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { dash_cooldown: 20, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .dash_cooldown,
            20,
            "--dash-cooldown 20 threads the cadence into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_dash_cooldown_into_a_matchmade_match() {
        // FM3 (path skew): --dash-cooldown must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // cadence a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { dash_cooldown: 20, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().dash_cooldown,
            20,
            "the matchmaker forms under --dash-cooldown 20 (matchmade == hand-seated)"
        );

        // No flag still forms dash_cooldown 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().dash_cooldown,
            0,
            "no --dash-cooldown: the matchmaker forms a disabled dash"
        );
    }

    #[test]
    fn direct_match_threads_pawn_radius_into_rules() {
        // FM1 (default drift): no --pawn-radius is 0 — pawns are not obstacles to one another, byte-identical
        // to the pre-flag harness (and its replay digest). The occupancy BEHAVIOR (a step onto another pawn's
        // cell is refused) is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().pawn_radius,
            0,
            "no --pawn-radius is 0 (occupancy off, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { pawn_radius: 750, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .pawn_radius,
            750,
            "--pawn-radius 750 threads the occupancy radius into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_pawn_radius_into_a_matchmade_match() {
        // FM3 (path skew): --pawn-radius must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // occupancy radius a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { pawn_radius: 750, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().pawn_radius,
            750,
            "the matchmaker forms under --pawn-radius 750 (matchmade == hand-seated)"
        );

        // No flag still forms pawn_radius 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().pawn_radius,
            0,
            "no --pawn-radius: the matchmaker forms disabled occupancy"
        );
    }

    #[test]
    fn direct_match_threads_pawn_height_into_rules() {
        // FM1 (default drift): no --pawn-height is 0 — occupancy is planar (z ignored), byte-identical to
        // the pre-flag harness (and its replay digest). The vault BEHAVIOR (a high-enough jump clears a
        // body band) is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().pawn_height,
            0,
            "no --pawn-height is 0 (planar occupancy, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { pawn_height: 1800, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .pawn_height,
            1800,
            "--pawn-height 1800 threads the occupancy band into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_pawn_height_into_a_matchmade_match() {
        // FM3 (path skew): --pawn-height must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // occupancy band a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { pawn_height: 1800, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().pawn_height,
            1800,
            "the matchmaker forms under --pawn-height 1800 (matchmade == hand-seated)"
        );

        // No flag still forms pawn_height 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().pawn_height,
            0,
            "no --pawn-height: the matchmaker forms planar occupancy"
        );
    }

    #[test]
    fn max_shield_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --max-shield pulls exactly one token and parses it
        // as the u16 cap, consuming no following flag. A --max-shield 50 right before --seats 3 must parse
        // BOTH (50 cap, seats 3); absent, it defaults 0 (shield off).
        let parsed =
            parse_args_from(["--max-shield", "50", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.max_shield, 50, "--max-shield 50 parses the cap");
        assert_eq!(parsed.seats, 3, "--max-shield consumed exactly one token, so --seats 3 parsed");

        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(none.max_shield, 0, "no --max-shield defaults 0 (shield disabled)");
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn max_shield_rejects_an_overflow() {
        // FM2 (type bound): max_shield is a u16; a value past u16::MAX must abort at the CLI, NOT wrap
        // into a small cap (65536 would wrap to 0 — which ALSO reads as "shield off", so the wrap is
        // doubly wrong). The u16 parse catches it; a negative aborts the same way ('-' is not a u16 digit).
        parse_args_from(["--max-shield", "65536"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_max_shield_into_rules() {
        // FM1 (default drift): no --max-shield is 0 — shield disabled, a Shield pickup inert,
        // byte-identical to the pre-flag harness (and its replay digest). The shield BEHAVIOR (a pool
        // drains before health, capped here) is arena-core's own test; here we pin the wiring via the
        // rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().max_shield,
            0,
            "no --max-shield is 0 (shield off, byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { max_shield: 50, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .max_shield,
            50,
            "--max-shield 50 threads the cap into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_max_shield_into_a_matchmade_match() {
        // FM3 (path skew): --max-shield must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // cap a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { max_shield: 50, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().max_shield,
            50,
            "the matchmaker forms under --max-shield 50 (matchmade == hand-seated)"
        );

        // No flag still forms max_shield 0 — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(off.rules().max_shield, 0, "no --max-shield: the matchmaker forms disabled shield");
    }

    #[test]
    fn start_health_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --start-health pulls exactly one token and parses
        // it as the u16 HP, consuming no following flag. A --start-health 50 right before --seats 3 must
        // parse BOTH (50 hp, seats 3).
        let parsed =
            parse_args_from(["--start-health", "50", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.start_health, 50, "--start-health 50 parses the HP");
        assert_eq!(parsed.seats, 3, "--start-health consumed exactly one token, so --seats 3 parsed");

        // FM (non-zero default): UNLIKE the feature-toggle knobs, an absent --start-health is NOT 0 — it
        // is the Rules default (a 0 would spawn an already-downed pawn, not the pre-flag behaviour).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.start_health,
            Rules::default().start_health,
            "no --start-health defaults to the Rules default HP, NOT 0"
        );
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn start_health_rejects_an_overflow() {
        // FM2 (type bound): start_health is a u16; a value past u16::MAX must abort at the CLI, NOT wrap
        // into a small pool (65536 would wrap to 0 — an already-downed spawn). The u16 parse catches it;
        // a negative aborts the same way ('-' is not a u16 digit).
        parse_args_from(["--start-health", "65536"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_start_health_into_rules() {
        // FM1 (default drift): no --start-health is the Rules DEFAULT HP — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().start_health,
            Rules::default().start_health,
            "no --start-health is the Rules default HP (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { start_health: 50, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .start_health,
            50,
            "--start-health 50 threads the HP into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_start_health_into_a_matchmade_match() {
        // FM3 (path skew): --start-health must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same HP
        // a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { start_health: 50, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().start_health,
            50,
            "the matchmaker forms under --start-health 50 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default HP — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().start_health,
            Rules::default().start_health,
            "no --start-health: the matchmaker forms the Rules default HP"
        );
    }

    #[test]
    fn damage_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --damage pulls exactly one token and parses it as
        // the u16 per-shot HP, consuming no following flag. A --damage 40 right before --seats 3 must
        // parse BOTH (40 damage, seats 3).
        let parsed = parse_args_from(["--damage", "40", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.damage, 40, "--damage 40 parses the per-shot HP");
        assert_eq!(parsed.seats, 3, "--damage consumed exactly one token, so --seats 3 parsed");

        // FM (non-zero default): UNLIKE the feature-toggle knobs, an absent --damage is NOT 0 — it is the
        // Rules default (a 0-damage shot could never down a pawn, not the pre-flag behaviour).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.damage,
            Rules::default().damage,
            "no --damage defaults to the Rules default damage, NOT 0"
        );
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn damage_rejects_an_overflow() {
        // FM2 (type bound): damage is a u16; a value past u16::MAX must abort at the CLI, NOT wrap into a
        // small (or zero) per-shot value (65536 would wrap to 0 — a shot that can never down a pawn). The
        // u16 parse catches it; a negative aborts the same way ('-' is not a u16 digit).
        parse_args_from(["--damage", "65536"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_damage_into_rules() {
        // FM1 (default drift): no --damage is the Rules DEFAULT damage — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().damage,
            Rules::default().damage,
            "no --damage is the Rules default damage (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { damage: 40, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .damage,
            40,
            "--damage 40 threads the per-shot HP into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_damage_into_a_matchmade_match() {
        // FM3 (path skew): --damage must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // per-shot damage a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { damage: 40, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().damage,
            40,
            "the matchmaker forms under --damage 40 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default damage — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().damage,
            Rules::default().damage,
            "no --damage: the matchmaker forms the Rules default damage"
        );
    }

    #[test]
    fn fire_cooldown_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --fire-cooldown pulls exactly one token and parses it
        // as the u16 inter-shot tick gate, consuming no following flag. A --fire-cooldown 3 right before
        // --seats 3 must parse BOTH (3 ticks, seats 3).
        let parsed = parse_args_from(["--fire-cooldown", "3", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.fire_cooldown, 3, "--fire-cooldown 3 parses the inter-shot tick gate");
        assert_eq!(parsed.seats, 3, "--fire-cooldown consumed exactly one token, so --seats 3 parsed");

        // FM (non-zero default): UNLIKE the feature-toggle knobs, an absent --fire-cooldown is NOT 0 — it is
        // the Rules default (a 0-cooldown pawn fires every tick, the degenerate case, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.fire_cooldown,
            Rules::default().fire_cooldown,
            "no --fire-cooldown defaults to the Rules default cadence, NOT 0"
        );
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn fire_cooldown_rejects_an_overflow() {
        // FM2 (type bound): fire_cooldown is a u16; a value past u16::MAX must abort at the CLI, NOT wrap into
        // a small (or zero) cadence (65536 would wrap to 0 — a pawn that fires every tick). The u16 parse
        // catches it; a negative aborts the same way ('-' is not a u16 digit).
        parse_args_from(["--fire-cooldown", "65536"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_fire_cooldown_into_rules() {
        // FM1 (default drift): no --fire-cooldown is the Rules DEFAULT cadence — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().fire_cooldown,
            Rules::default().fire_cooldown,
            "no --fire-cooldown is the Rules default cadence (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { fire_cooldown: 3, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .fire_cooldown,
            3,
            "--fire-cooldown 3 threads the inter-shot gate into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_fire_cooldown_into_a_matchmade_match() {
        // FM3 (path skew): --fire-cooldown must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // fire cadence a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { fire_cooldown: 3, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().fire_cooldown,
            3,
            "the matchmaker forms under --fire-cooldown 3 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default cadence — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().fire_cooldown,
            Rules::default().fire_cooldown,
            "no --fire-cooldown: the matchmaker forms the Rules default cadence"
        );
    }

    #[test]
    fn mag_size_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --mag-size pulls exactly one token and parses it as the
        // u16 magazine capacity, consuming no following flag. A --mag-size 10 right before --seats 3 must
        // parse BOTH (10 ammo, seats 3).
        let parsed = parse_args_from(["--mag-size", "10", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.mag_size, 10, "--mag-size 10 parses the magazine capacity");
        assert_eq!(parsed.seats, 3, "--mag-size consumed exactly one token, so --seats 3 parsed");

        // FM (non-zero default): UNLIKE the feature-toggle knobs, an absent --mag-size is NOT 0 — it is the
        // Rules default (a 0-mag pawn spawns empty and can never fire a ranged shot, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.mag_size,
            Rules::default().mag_size,
            "no --mag-size defaults to the Rules default magazine, NOT 0"
        );
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn mag_size_rejects_an_overflow() {
        // FM2 (type bound): mag_size is a u16; a value past u16::MAX must abort at the CLI, NOT wrap into a
        // small (or zero) magazine (65536 would wrap to 0 — an empty pawn that can never fire). The u16 parse
        // catches it; a negative aborts the same way ('-' is not a u16 digit).
        parse_args_from(["--mag-size", "65536"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_mag_size_into_rules() {
        // FM1 (default drift): no --mag-size is the Rules DEFAULT capacity — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().mag_size,
            Rules::default().mag_size,
            "no --mag-size is the Rules default magazine (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { mag_size: 10, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .mag_size,
            10,
            "--mag-size 10 threads the magazine capacity into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_mag_size_into_a_matchmade_match() {
        // FM3 (path skew): --mag-size must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // magazine a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { mag_size: 10, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().mag_size,
            10,
            "the matchmaker forms under --mag-size 10 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default magazine — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().mag_size,
            Rules::default().mag_size,
            "no --mag-size: the matchmaker forms the Rules default magazine"
        );
    }

    #[test]
    fn parse_max_speed_maps_a_non_negative_magnitude() {
        assert_eq!(parse_max_speed("0"), 0);
        assert_eq!(parse_max_speed("400"), 400);
        assert_eq!(
            parse_max_speed(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid walk magnitude"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_max_speed_rejects_a_negative() {
        // Core slides a full-intent walk by max_speed position units; it has no meaning for a negative
        // (it never walks a pawn backward by the cap), so a forwarded negative is a footgun. Reject it
        // loudly at the CLI; the u32 parse fails the leading '-'.
        parse_max_speed("-400");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_max_speed_rejects_an_overflow() {
        // A magnitude past i32::MAX must abort, NOT wrap into a negative (which core has no movement
        // meaning for). 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_max_speed("3000000000");
    }

    #[test]
    fn max_speed_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --max-speed pulls exactly one token (through
        // parse_max_speed) and consumes no following flag. A --max-speed 400 right before --seats 3
        // must parse BOTH.
        let parsed = parse_args_from(["--max-speed", "400", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.max_speed, 400, "--max-speed 400 parses the walk magnitude");
        assert_eq!(parsed.seats, 3, "--max-speed consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --max-speed is NOT 0 — it is
        // the Rules default (a 0-speed pawn is frozen in place, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.max_speed,
            Rules::default().max_speed,
            "no --max-speed defaults to the Rules default pace, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_max_speed_into_rules() {
        // FM1 (default drift): no --max-speed is the Rules DEFAULT pace — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().max_speed,
            Rules::default().max_speed,
            "no --max-speed is the Rules default pace (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { max_speed: 400, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .max_speed,
            400,
            "--max-speed 400 threads the walk magnitude into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_max_speed_into_a_matchmade_match() {
        // FM3 (path skew): --max-speed must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // pace a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { max_speed: 400, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().max_speed,
            400,
            "the matchmaker forms under --max-speed 400 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default pace — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().max_speed,
            Rules::default().max_speed,
            "no --max-speed: the matchmaker forms the Rules default pace"
        );
    }

    #[test]
    fn parse_perception_range_maps_a_non_negative_radius() {
        assert_eq!(parse_perception_range("0"), 0);
        assert_eq!(parse_perception_range("20000"), 20000);
        assert_eq!(
            parse_perception_range(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid perception radius"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_perception_range_rejects_a_negative() {
        // A perception radius is non-negative; a negative is meaningless (it would perceive nothing).
        // Reject it loudly at the CLI; the u32 parse fails the leading '-'.
        parse_perception_range("-20000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_perception_range_rejects_an_overflow() {
        // A radius past i32::MAX must abort, NOT wrap into a negative (which would then blind every seat).
        // 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_perception_range("3000000000");
    }

    #[test]
    fn perception_range_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --perception-range pulls exactly one token (through
        // parse_perception_range) and consumes no following flag. A --perception-range 20000 right before
        // --seats 3 must parse BOTH.
        let parsed =
            parse_args_from(["--perception-range", "20000", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.perception_range, 20000, "--perception-range 20000 parses the radius");
        assert_eq!(parsed.seats, 3, "--perception-range consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --perception-range is NOT 0 — it
        // is the Rules default (a 0-range seat is blind, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.perception_range,
            Rules::default().perception_range,
            "no --perception-range defaults to the Rules default radius, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_perception_range_into_rules() {
        // FM1 (default drift): no --perception-range is the Rules DEFAULT radius — NOT 0 — byte-identical to
        // the pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().perception_range,
            Rules::default().perception_range,
            "no --perception-range is the Rules default radius (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { perception_range: 20000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .perception_range,
            20000,
            "--perception-range 20000 threads the radius into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_perception_range_into_a_matchmade_match() {
        // FM3 (path skew): --perception-range must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // radius a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { perception_range: 20000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().perception_range,
            20000,
            "the matchmaker forms under --perception-range 20000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default radius — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().perception_range,
            Rules::default().perception_range,
            "no --perception-range: the matchmaker forms the Rules default radius"
        );
    }

    #[test]
    fn parse_weapon_range_maps_a_non_negative_reach() {
        assert_eq!(parse_weapon_range("0"), 0);
        assert_eq!(parse_weapon_range("15000"), 15000);
        assert_eq!(
            parse_weapon_range(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid weapon reach"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_weapon_range_rejects_a_negative() {
        // A weapon reach is non-negative; a negative is meaningless (it would land no hit). Reject it loudly
        // at the CLI; the u32 parse fails the leading '-'.
        parse_weapon_range("-15000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_weapon_range_rejects_an_overflow() {
        // A reach past i32::MAX must abort, NOT wrap into a negative (which would then disarm every ranged
        // shot). 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_weapon_range("3000000000");
    }

    #[test]
    fn weapon_range_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --weapon-range pulls exactly one token (through
        // parse_weapon_range) and consumes no following flag. A --weapon-range 15000 right before --seats 3
        // must parse BOTH.
        let parsed =
            parse_args_from(["--weapon-range", "15000", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.weapon_range, 15000, "--weapon-range 15000 parses the reach");
        assert_eq!(parsed.seats, 3, "--weapon-range consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --weapon-range is NOT 0 — it is
        // the Rules default (a 0-range weapon reaches nothing, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.weapon_range,
            Rules::default().weapon_range,
            "no --weapon-range defaults to the Rules default reach, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_weapon_range_into_rules() {
        // FM1 (default drift): no --weapon-range is the Rules DEFAULT reach — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().weapon_range,
            Rules::default().weapon_range,
            "no --weapon-range is the Rules default reach (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { weapon_range: 15000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .weapon_range,
            15000,
            "--weapon-range 15000 threads the reach into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_weapon_range_into_a_matchmade_match() {
        // FM3 (path skew): --weapon-range must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same reach
        // a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { weapon_range: 15000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().weapon_range,
            15000,
            "the matchmaker forms under --weapon-range 15000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default reach — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().weapon_range,
            Rules::default().weapon_range,
            "no --weapon-range: the matchmaker forms the Rules default reach"
        );
    }

    #[test]
    fn parse_hit_radius_maps_a_non_negative_radius() {
        assert_eq!(parse_hit_radius("0"), 0);
        assert_eq!(parse_hit_radius("3000"), 3000);
        assert_eq!(
            parse_hit_radius(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid beam half-width"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_hit_radius_rejects_a_negative() {
        // A beam half-width is non-negative; a negative is meaningless (it would land no hit). Reject it
        // loudly at the CLI; the u32 parse fails the leading '-'.
        parse_hit_radius("-3000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_hit_radius_rejects_an_overflow() {
        // A half-width past i32::MAX must abort, NOT wrap into a negative (which would then miss every shot).
        // 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_hit_radius("3000000000");
    }

    #[test]
    fn hit_radius_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --hit-radius pulls exactly one token (through
        // parse_hit_radius) and consumes no following flag. A --hit-radius 3000 right before --seats 3
        // must parse BOTH.
        let parsed =
            parse_args_from(["--hit-radius", "3000", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.hit_radius, 3000, "--hit-radius 3000 parses the half-width");
        assert_eq!(parsed.seats, 3, "--hit-radius consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --hit-radius is NOT 0 — it is
        // the Rules default (a 0 radius is a needle-thin beam landing only on a dead-centre target, not the
        // pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.hit_radius,
            Rules::default().hit_radius,
            "no --hit-radius defaults to the Rules default radius, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_hit_radius_into_rules() {
        // FM1 (default drift): no --hit-radius is the Rules DEFAULT radius — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().hit_radius,
            Rules::default().hit_radius,
            "no --hit-radius is the Rules default radius (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { hit_radius: 3000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .hit_radius,
            3000,
            "--hit-radius 3000 threads the half-width into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_hit_radius_into_a_matchmade_match() {
        // FM3 (path skew): --hit-radius must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // half-width a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { hit_radius: 3000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().hit_radius,
            3000,
            "the matchmaker forms under --hit-radius 3000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default radius — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().hit_radius,
            Rules::default().hit_radius,
            "no --hit-radius: the matchmaker forms the Rules default radius"
        );
    }

    #[test]
    fn melee_cooldown_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --melee-cooldown pulls exactly one token and parses it
        // as the u16 swing cadence, consuming no following flag. A --melee-cooldown 30 right before --seats 3
        // must parse BOTH.
        let parsed = parse_args_from(
            ["--melee-cooldown", "30", "--seats", "3"].into_iter().map(String::from),
        );
        assert_eq!(parsed.melee_cooldown, 30, "--melee-cooldown 30 parses the cadence");
        assert_eq!(parsed.seats, 3, "--melee-cooldown consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --melee-cooldown is NOT 0 — it is
        // the Rules default (a 0 cooldown swings every tick, a continuous cleave, not the pre-flag behaviour).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.melee_cooldown,
            Rules::default().melee_cooldown,
            "no --melee-cooldown defaults to the Rules default cadence, NOT 0"
        );

        // FM1 (explicit 0 forwards): an explicitly requested 0 is the continuous-cleave degenerate — it must
        // forward verbatim, NOT be coerced back to the default. Only an ABSENT flag is the default.
        let zero =
            parse_args_from(["--melee-cooldown", "0", "--seats", "2"].into_iter().map(String::from));
        assert_eq!(zero.melee_cooldown, 0, "an explicit --melee-cooldown 0 forwards (continuous cleave)");
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn melee_cooldown_rejects_a_negative() {
        // FM2 (type bound): melee_cooldown is a u16; a negative is meaningless for a tick count and must abort
        // at the CLI ('-' is not a u16 digit), NOT silently coerce. A value past u16::MAX aborts the same way.
        parse_args_from(["--melee-cooldown", "-5"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_melee_cooldown_into_rules() {
        // FM1 (default drift): no --melee-cooldown is the Rules DEFAULT cadence — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the feature-toggle
        // knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().melee_cooldown,
            Rules::default().melee_cooldown,
            "no --melee-cooldown is the Rules default cadence (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { melee_cooldown: 30, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .melee_cooldown,
            30,
            "--melee-cooldown 30 threads the cadence into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_melee_cooldown_into_a_matchmade_match() {
        // FM3 (path skew): --melee-cooldown must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same cadence
        // a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { melee_cooldown: 30, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().melee_cooldown,
            30,
            "the matchmaker forms under --melee-cooldown 30 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default cadence — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().melee_cooldown,
            Rules::default().melee_cooldown,
            "no --melee-cooldown: the matchmaker forms the Rules default cadence"
        );
    }

    #[test]
    fn melee_damage_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --melee-damage pulls exactly one token and parses it as
        // the u16 per-swing damage, consuming no following flag. A --melee-damage 70 right before --seats 3 must
        // parse BOTH.
        let parsed =
            parse_args_from(["--melee-damage", "70", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.melee_damage, 70, "--melee-damage 70 parses the per-swing damage");
        assert_eq!(parsed.seats, 3, "--melee-damage consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --melee-damage is NOT 0 — it is the
        // Rules default (a 0-damage swing never harms, a harmless melee pawn, not the pre-flag behaviour).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.melee_damage,
            Rules::default().melee_damage,
            "no --melee-damage defaults to the Rules default damage, NOT 0"
        );

        // FM1 (explicit 0 forwards): an explicitly requested 0 is the harmless-swing degenerate — it must forward
        // verbatim, NOT be coerced back to the default. Only an ABSENT flag is the default.
        let zero =
            parse_args_from(["--melee-damage", "0", "--seats", "2"].into_iter().map(String::from));
        assert_eq!(zero.melee_damage, 0, "an explicit --melee-damage 0 forwards (a harmless swing)");
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn melee_damage_rejects_a_negative() {
        // FM2 (type bound): melee_damage is a u16; a negative is meaningless for a damage amount and must abort
        // at the CLI ('-' is not a u16 digit), NOT silently coerce. A value past u16::MAX aborts the same way.
        parse_args_from(["--melee-damage", "-5"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_melee_damage_into_rules() {
        // FM1 (default drift): no --melee-damage is the Rules DEFAULT damage — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the feature-toggle
        // knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().melee_damage,
            Rules::default().melee_damage,
            "no --melee-damage is the Rules default damage (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { melee_damage: 70, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .melee_damage,
            70,
            "--melee-damage 70 threads the damage into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_melee_damage_into_a_matchmade_match() {
        // FM3 (path skew): --melee-damage must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same damage a
        // hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { melee_damage: 70, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().melee_damage,
            70,
            "the matchmaker forms under --melee-damage 70 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default damage — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().melee_damage,
            Rules::default().melee_damage,
            "no --melee-damage: the matchmaker forms the Rules default damage"
        );
    }

    #[test]
    fn parse_melee_range_maps_a_non_negative_reach() {
        assert_eq!(parse_melee_range("0"), 0);
        assert_eq!(parse_melee_range("8000"), 8000);
        assert_eq!(
            parse_melee_range(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid cleave reach"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_melee_range_rejects_a_negative() {
        // A cleave reach is non-negative; a negative is meaningless (it would cleave nothing). Reject it
        // loudly at the CLI; the u32 parse fails the leading '-'.
        parse_melee_range("-8000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_melee_range_rejects_an_overflow() {
        // A reach past i32::MAX must abort, NOT wrap into a negative (which would then cleave nothing).
        // 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_melee_range("3000000000");
    }

    #[test]
    fn melee_range_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --melee-range pulls exactly one token (through
        // parse_melee_range) and consumes no following flag. A --melee-range 8000 right before --seats 3
        // must parse BOTH.
        let parsed =
            parse_args_from(["--melee-range", "8000", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.melee_range, 8000, "--melee-range 8000 parses the reach");
        assert_eq!(parsed.seats, 3, "--melee-range consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --melee-range is NOT 0 — it is
        // the Rules default (a 0 reach cleaves only an enemy exactly on the shooter, a harmless melee pawn,
        // not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.melee_range,
            Rules::default().melee_range,
            "no --melee-range defaults to the Rules default reach, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_melee_range_into_rules() {
        // FM1 (default drift): no --melee-range is the Rules DEFAULT reach — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the
        // feature-toggle knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().melee_range,
            Rules::default().melee_range,
            "no --melee-range is the Rules default reach (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { melee_range: 8000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .melee_range,
            8000,
            "--melee-range 8000 threads the reach into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_melee_range_into_a_matchmade_match() {
        // FM3 (path skew): --melee-range must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same
        // reach a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { melee_range: 8000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().melee_range,
            8000,
            "the matchmaker forms under --melee-range 8000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default reach — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().melee_range,
            Rules::default().melee_range,
            "no --melee-range: the matchmaker forms the Rules default reach"
        );
    }

    #[test]
    fn parse_projectile_speed_maps_a_non_negative_speed() {
        assert_eq!(parse_projectile_speed("0"), 0);
        assert_eq!(parse_projectile_speed("8000"), 8000);
        assert_eq!(
            parse_projectile_speed(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid travel speed"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_projectile_speed_rejects_a_negative() {
        // A travel speed is non-negative; a negative is meaningless (core flies a projectile forward along the
        // octant, never backward). Reject it loudly at the CLI; the u32 parse fails the leading '-'.
        parse_projectile_speed("-8000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_projectile_speed_rejects_an_overflow() {
        // A speed past i32::MAX must abort, NOT wrap into a negative (which would then fly nothing).
        // 3_000_000_000 fits a u32 but not an i32, so i32::try_from catches it.
        parse_projectile_speed("3000000000");
    }

    #[test]
    fn projectile_speed_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --projectile-speed pulls exactly one token (through
        // parse_projectile_speed) and consumes no following flag. A --projectile-speed 8000 right before
        // --seats 3 must parse BOTH.
        let parsed = parse_args_from(
            ["--projectile-speed", "8000", "--seats", "3"].into_iter().map(String::from),
        );
        assert_eq!(parsed.projectile_speed, 8000, "--projectile-speed 8000 parses the speed");
        assert_eq!(
            parsed.seats, 3,
            "--projectile-speed consumed exactly one token, so --seats 3 parsed"
        );

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --projectile-speed is NOT 0 — it is
        // the Rules default (a 0-speed shot never leaves the muzzle and is force-expired landing no hit, not the
        // pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.projectile_speed,
            Rules::default().projectile_speed,
            "no --projectile-speed defaults to the Rules default speed, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_projectile_speed_into_rules() {
        // FM1 (default drift): no --projectile-speed is the Rules DEFAULT speed — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). This is the base-balance distinction from the feature-toggle
        // knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().projectile_speed,
            Rules::default().projectile_speed,
            "no --projectile-speed is the Rules default speed (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { projectile_speed: 8000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .projectile_speed,
            8000,
            "--projectile-speed 8000 threads the speed into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_projectile_speed_into_a_matchmade_match() {
        // FM3 (path skew): --projectile-speed must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same speed a
        // hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { projectile_speed: 8000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().projectile_speed,
            8000,
            "the matchmaker forms under --projectile-speed 8000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default speed — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().projectile_speed,
            Rules::default().projectile_speed,
            "no --projectile-speed: the matchmaker forms the Rules default speed"
        );
    }

    #[test]
    fn action_deadline_micros_parses_as_a_u32_value_flag() {
        // The value-flag twin of the threading tests: --action-deadline-micros pulls exactly one token (a plain
        // u32 .parse()) and consumes no following flag. A --action-deadline-micros 100000 right before --seats 3
        // must parse BOTH.
        let parsed = parse_args_from(
            ["--action-deadline-micros", "100000", "--seats", "3"].into_iter().map(String::from),
        );
        assert_eq!(parsed.action_deadline_micros, 100000, "--action-deadline-micros 100000 parses the budget");
        assert_eq!(parsed.seats, 3, "--action-deadline-micros consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --action-deadline-micros is NOT 0 —
        // it is the Rules default (a 0 budget gives a seat no time to act, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.action_deadline_micros,
            Rules::default().action_deadline_micros,
            "no --action-deadline-micros defaults to the Rules default budget, NOT 0"
        );
    }

    #[test]
    fn enforce_deadline_is_a_valueless_opt_in_off_by_default() {
        // The enforcement toggle is a bare boolean flag (consumes no token), OFF by default — off keeps the
        // unbounded blocking read so the golden/replay path stays timer-free. A --enforce-deadline before --seats 3
        // sets the toggle and still parses --seats (it pulled no value).
        let on = parse_args_from(["--enforce-deadline", "--seats", "3"].into_iter().map(String::from));
        assert!(on.enforce_deadline, "--enforce-deadline arms the wall-clock read budget");
        assert_eq!(on.seats, 3, "--enforce-deadline consumed no token, so --seats 3 parsed");

        let off = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert!(!off.enforce_deadline, "no --enforce-deadline leaves the blocking, timer-free read (the default)");
    }

    #[test]
    #[should_panic(expected = "u32")]
    fn action_deadline_micros_rejects_a_negative() {
        // A microsecond budget is non-negative; the plain u32 parse fails the leading '-' (and bounds the value
        // at u32::MAX, so an overflow aborts on the same parse). Reject it loudly at the CLI.
        parse_args_from(["--action-deadline-micros", "-100000"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_action_deadline_micros_into_rules() {
        // FM1 (default drift): no --action-deadline-micros is the Rules DEFAULT budget — NOT 0 — byte-identical
        // to the pre-flag harness (and its replay digest). Pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().action_deadline_micros,
            Rules::default().action_deadline_micros,
            "no --action-deadline-micros is the Rules default budget (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(
                &Args { action_deadline_micros: 100000, ..direct_args(2, "reference", 0) },
                2
            )
            .rules()
            .action_deadline_micros,
            100000,
            "--action-deadline-micros 100000 threads the budget into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_action_deadline_micros_into_a_matchmade_match() {
        // FM3 (path skew): --action-deadline-micros must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same budget a
        // hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { action_deadline_micros: 100000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().action_deadline_micros,
            100000,
            "the matchmaker forms under --action-deadline-micros 100000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default budget — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().action_deadline_micros,
            Rules::default().action_deadline_micros,
            "no --action-deadline-micros: the matchmaker forms the Rules default budget"
        );
    }

    fn idle_intent() -> ActionIntent {
        ActionIntent {
            move_dir: Vec2 { x: 0, y: 0 },
            aim: 0,
            buttons: ActionButtons { fire: false, jump: false, ability: false, reload: false },
        }
    }

    /// A transport-framed Act line for `seat` answering `tick` of `match_id`, exactly as an
    /// agent sends it — the envelope [`emit`] writes and [`read_agent`] parses back. The input
    /// twin of `emit`, so a test can feed the deadline-enforced read the way a real client would.
    fn act_line(seat: SeatId, match_id: Uuid, tick: u64) -> String {
        let action =
            Action { protocol_version: PROTOCOL_VERSION, match_id, seat, tick, intent: idle_intent() };
        let frame = serde_json::to_value(AgentMsg::Act(action)).expect("serialize act");
        let envelope = serde_json::json!({ "seat": seat, "frame": frame });
        serde_json::to_string(&envelope).expect("serialize envelope")
    }

    /// A transport-framed Leave for `seat` — the envelope [`read_agent`] parses back. The
    /// seat travels in the envelope (as for every agent frame), not the `Leave` body, so a
    /// forged body cannot depart a seat it does not own.
    fn leave_line(seat: SeatId) -> String {
        let frame = serde_json::to_value(AgentMsg::Leave { reason: "forfeit".into() }).expect("serialize leave");
        let envelope = serde_json::json!({ "seat": seat, "frame": frame });
        serde_json::to_string(&envelope).expect("serialize envelope")
    }

    /// The full active roster `0..n` — the starting set both pump loops seed before the first
    /// Live tick, and what the deadline read expects a line from each tick until a seat leaves.
    fn active_seats(n: u8) -> BTreeSet<SeatId> {
        (0..n).collect()
    }

    /// A `pump_to_end` input stream from a per-line script, framed the way the handshake's
    /// stdin iterator yields lines. The blocking pump reads exactly what an agent would send.
    fn pump_input(lines: &[String]) -> io::Lines<io::BufReader<io::Cursor<String>>> {
        io::BufReader::new(io::Cursor::new(format!("{}\n", lines.join("\n")))).lines()
    }

    /// The seats each recorded tick ingested, in ascending order — the transport-level proof
    /// of who acted and who forfeited: a departed seat never reappears here after its Leave.
    fn tick_seats(replay: &ReplayRecord) -> Vec<(u64, Vec<SeatId>)> {
        replay
            .ticks
            .iter()
            .map(|t| (t.tick, t.actions.iter().map(|a| a.seat).collect()))
            .collect()
    }

    /// The seats that FORFEITED at each tick that had any — the leave stream, so a test
    /// can pin exactly when a Leave eliminated its seat (and that a no-Leave match has none).
    fn tick_forfeits(replay: &ReplayRecord) -> Vec<(u64, Vec<SeatId>)> {
        replay
            .ticks
            .iter()
            .filter(|t| !t.forfeits.is_empty())
            .map(|t| (t.tick, t.forfeits.clone()))
            .collect()
    }

    #[test]
    fn pump_to_end_records_every_seat_every_tick_with_no_leave() {
        // FM4 inert baseline: a match with no Leave ingests both seats on every one of its
        // max_ticks ticks — the byte-identical behaviour the departure cases diverge from.
        // direct_args caps max_ticks at 4, so the idle 1v1 (both alive, distinct teams) runs
        // to the timeout with both seats acting throughout.
        let mut m = build_direct_match(&direct_args(2, "", 0), 2);
        let mid = m.match_id();
        let script: Vec<String> =
            (0..4).flat_map(|t| [act_line(0, mid, t), act_line(1, mid, t)]).collect();
        let mut out: Vec<u8> = Vec::new();
        let result = pump_to_end(&mut m, 2, &mut pump_input(&script), &mut out);

        assert_eq!(m.phase(), MatchPhase::Ended);
        assert_eq!(result.outcomes.len(), 2);
        assert_eq!(
            tick_seats(&m.into_replay()),
            vec![(0, vec![0, 1]), (1, vec![0, 1]), (2, vec![0, 1]), (3, vec![0, 1])],
            "no Leave: every tick ingests both seats"
        );
    }

    #[test]
    fn pump_to_end_forfeits_a_leaver_from_its_leave_tick_and_still_ends() {
        // FM1: seat 1 acts at tick 0, then Leaves at tick 1. The Leave is a durable FORFEIT
        // — seat 1 is ELIMINATED at tick 1, so this 1v1 ENDS there (seat 0 is left alone),
        // not idling to the max_ticks cap. seat 0's queued tick-2/3 lines are never read.
        let mut m = build_direct_match(&direct_args(2, "", 0), 2);
        let mid = m.match_id();
        let script = [
            act_line(0, mid, 0),
            act_line(1, mid, 0),
            act_line(0, mid, 1),
            leave_line(1),       // seat 1 forfeits at tick 1
            act_line(0, mid, 2), // never read — the match has already ended
            act_line(0, mid, 3),
        ];
        let mut out: Vec<u8> = Vec::new();
        let result = pump_to_end(&mut m, 2, &mut pump_input(&script), &mut out);

        assert_eq!(m.phase(), MatchPhase::Ended, "the leaver's elimination ends the 1v1");
        assert_eq!(result.final_tick, 2, "it ends at the leave tick, not the max_ticks cap");
        let replay = m.into_replay();
        assert_eq!(
            tick_seats(&replay),
            vec![(0, vec![0, 1]), (1, vec![0])],
            "seat 1 acts only at tick 0; at its tick-1 Leave it forfeits and the match ends"
        );
        assert_eq!(tick_forfeits(&replay), vec![(1, vec![1])], "seat 1 is recorded as forfeiting at tick 1");
        let s0 = result.outcomes.iter().find(|o| o.seat == 0).unwrap();
        let s1 = result.outcomes.iter().find(|o| o.seat == 1).unwrap();
        assert_eq!((s0.placement, s0.alive_at_end), (1, true), "the seat that stayed wins");
        assert_eq!((s1.placement, s1.alive_at_end), (2, false), "the leaver is eliminated, placed last");
    }

    #[test]
    fn pump_to_end_ignores_a_post_leave_act_from_the_departed_seat() {
        // FM2 (read-loop alignment): seat 1 Leaves at tick 0 and then (buggily) sends an Act
        // for tick 1. A 3-seat FFA keeps the match alive after the one departure (seats 0 and 2
        // fight on), so the stray line is actually reached: it is dropped WITHOUT consuming a
        // slot — the read stays aligned, so seats 0 AND 2 are still both recorded at tick 1 —
        // and is never ingested. Seat 1 forfeits exactly once (tick 0) and never reappears.
        let mut m = build_direct_match(&direct_args(3, "", 0), 3);
        let mid = m.match_id();
        let script = [
            act_line(0, mid, 0),
            leave_line(1), // seat 1 forfeits at tick 0
            act_line(2, mid, 0),
            act_line(1, mid, 1), // stray, well-formed Act from the departed seat — must be dropped
            act_line(0, mid, 1),
            act_line(2, mid, 1),
            act_line(0, mid, 2),
            act_line(2, mid, 2),
            act_line(0, mid, 3),
            act_line(2, mid, 3),
        ];
        let mut out: Vec<u8> = Vec::new();
        pump_to_end(&mut m, 3, &mut pump_input(&script), &mut out);

        assert_eq!(m.phase(), MatchPhase::Ended);
        let replay = m.into_replay();
        assert_eq!(
            tick_seats(&replay),
            vec![(0, vec![0, 2]), (1, vec![0, 2]), (2, vec![0, 2]), (3, vec![0, 2])],
            "the stray tick-1 line from departed seat 1 is dropped without desyncing seats 0/2"
        );
        assert_eq!(
            tick_forfeits(&replay),
            vec![(0, vec![1])],
            "seat 1 forfeits once at tick 0 and never reappears in either stream"
        );
    }

    #[test]
    fn pump_to_end_ends_cleanly_when_every_seat_leaves() {
        // FM3: both seats Leave at tick 0 → both are forfeited (eliminated) that tick, leaving
        // ZERO teams alive, so the match ends immediately at tick 0 with a well-formed result —
        // an empty active set does not deadlock the read or panic on EOF, and a mutual forfeit
        // does not stall to the max_ticks cap.
        let mut m = build_direct_match(&direct_args(2, "", 0), 2);
        let script = [leave_line(0), leave_line(1)];
        let mut out: Vec<u8> = Vec::new();
        let result = pump_to_end(&mut m, 2, &mut pump_input(&script), &mut out);

        assert_eq!(m.phase(), MatchPhase::Ended, "both departed: the match ends at once, not at the cap");
        assert_eq!(result.final_tick, 1, "both forfeit at tick 0, so the match ends there");
        assert_eq!(result.outcomes.len(), 2, "both seats are ranked");
        assert!(result.outcomes.iter().all(|o| !o.alive_at_end), "no one survives a mutual forfeit");
        let replay = m.into_replay();
        assert_eq!(tick_seats(&replay), vec![(0, vec![])], "no seat acts — the only tick is an empty forfeit");
        assert_eq!(tick_forfeits(&replay), vec![(0, vec![0, 1])], "both seats forfeit at tick 0");
    }

    #[test]
    fn read_tick_deadlined_honors_a_present_seat_and_forfeits_a_withheld_one() {
        // FM1/FM2: a seat whose action is already available is ingested; a seat that never
        // answers is omitted so the sim forfeits it. The sender stays alive across the read (the
        // stream is OPEN, just silent), so the withheld seat TIMES OUT — the wall-clock case, not
        // an EOF — firing deterministically since the line is never sent.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        assert_eq!(
            (m.phase(), m.tick()),
            (MatchPhase::Live, 0),
            "a no-countdown direct match opens Live at tick 0"
        );

        let (tx, rx) = mpsc::channel::<String>();
        tx.send(act_line(0, m.match_id(), 0)).unwrap(); // seat 0 answers; seat 1 is withheld

        let intents = read_tick_deadlined(&m, &mut active_seats(2), &rx, Duration::from_millis(50), &mut BTreeMap::new());
        assert!(intents.contains_key(&0), "the present seat 0 is ingested");
        assert!(!intents.contains_key(&1), "the withheld seat 1 is forfeited (omitted from intents)");
        drop(tx); // held open ACROSS the read above (so seat 1 timed out, not disconnected)
    }

    #[test]
    fn read_tick_deadlined_ingests_every_present_seat_without_forfeit() {
        // The no-timeout case the golden path relies on: both seats' actions are already in the
        // channel, so both ingest and NOTHING forfeits — a present line under the budget never
        // times out — and the read returns well within the budget.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(act_line(0, m.match_id(), 0)).unwrap();
        tx.send(act_line(1, m.match_id(), 0)).unwrap();

        let start = Instant::now();
        let intents = read_tick_deadlined(&m, &mut active_seats(2), &rx, Duration::from_millis(50), &mut BTreeMap::new());
        assert_eq!(intents.len(), 2, "both present seats ingest");
        assert!(intents.contains_key(&0) && intents.contains_key(&1));
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "immediately-available lines never wait the budget (no spurious timeout)"
        );
        drop(tx);
    }

    #[test]
    fn read_tick_deadlined_forfeits_every_silent_seat_in_one_shared_read() {
        // FM4 (deterministic half): several silent seats in one tick ALL forfeit under a single
        // shared read — only the present seat acts. The shared per-tick clock + break-on-timeout
        // bound the wall-clock cost to ~one deadline rather than one sequential deadline per unread
        // seat, but that bound is inherently load-sensitive (a loaded box overshoots a 50 ms wait
        // several-fold), so it is asserted in the mutation proof, not as a flaky wall-clock check
        // here. This pins WHICH seats forfeit; the timing bound is proved by neutralising the
        // shared deadline and observing the n-fold slowdown.
        let m = build_direct_match(&direct_args(4, "", 0), 4);
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(act_line(0, m.match_id(), 0)).unwrap(); // seat 0 answers; seats 1..=3 are withheld

        let intents = read_tick_deadlined(&m, &mut active_seats(4), &rx, Duration::from_millis(50), &mut BTreeMap::new());
        assert_eq!(
            intents.keys().copied().collect::<Vec<_>>(),
            vec![0],
            "only the present seat acts; all three silent seats forfeit in the one shared read"
        );
        drop(tx);
    }

    #[test]
    fn read_tick_deadlined_omits_the_rest_on_a_closed_stream() {
        // FM3: a closed stream (EOF) is distinguished from a timeout — the remaining seats are
        // forfeited AT ONCE, not after waiting the budget, so a dead stream doesn't burn one
        // deadline per tick. Seat 0's line is buffered before the sender drops, so it is still
        // drained ahead of Disconnected.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(act_line(0, m.match_id(), 0)).unwrap();
        drop(tx); // EOF: the stream closes with seat 1 never sent

        let start = Instant::now();
        let intents = read_tick_deadlined(&m, &mut active_seats(2), &rx, Duration::from_millis(50), &mut BTreeMap::new());
        assert!(intents.contains_key(&0), "the buffered seat 0 line is still drained before EOF");
        assert!(!intents.contains_key(&1), "seat 1 is forfeited on the closed stream");
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "EOF forfeits the rest immediately, it does not wait the budget"
        );
    }

    #[test]
    fn pump_to_end_deadlined_forfeits_a_dead_stream_to_a_bounded_end() {
        // FM4 (no hang, no spin): a match whose stream is closed before a single action
        // ELIMINATES every silent seat at once (a closed stream is an immediate forfeit, not a
        // per-tick timeout), so it ends the moment no team survives — promptly at tick 1, NOT
        // idling to the max_ticks cap and NOT blocking forever on a dead read.
        let mut m = build_direct_match(&direct_args(2, "", 0), 2);
        let (tx, rx) = mpsc::channel::<String>();
        drop(tx); // the agent stream is closed before a single action

        let mut out: Vec<u8> = Vec::new();
        let result = pump_to_end_deadlined(&mut m, 2, &rx, &mut out, Duration::from_millis(50));
        assert_eq!(
            m.phase(),
            MatchPhase::Ended,
            "the deadlined pump drives a dead-stream match to its end, it does not hang"
        );
        assert_eq!(result.final_tick, 1, "both seats forfeit the EOF tick, so it ends at tick 1, not the cap");
        assert_eq!(result.outcomes.len(), 2, "both seats are ranked in the terminal result");
        assert!(result.outcomes.iter().all(|o| !o.alive_at_end), "a dead stream downs both seats");
        let replay = m.into_replay();
        assert_eq!(tick_seats(&replay), vec![(0, vec![])], "no seat acts on a dead stream");
        assert_eq!(tick_forfeits(&replay), vec![(0, vec![0, 1])], "both seats forfeit at the tick-0 EOF");
    }

    #[test]
    fn read_tick_deadlined_departs_a_leaving_seat_so_it_is_no_longer_awaited() {
        // The enforced-path twin of the blocking departure: a Leave forfeits its seat AND drops
        // it from the active set, so the next tick's read expects only the survivors and never
        // waits the budget for the departed seat. Both lines are buffered, so the departure read
        // never times out.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        let mut active = active_seats(2);
        let mut misses = BTreeMap::new();
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(act_line(0, m.match_id(), 0)).unwrap();
        tx.send(leave_line(1)).unwrap();

        let intents = read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(50), &mut misses);
        assert!(intents.contains_key(&0), "seat 0 acted");
        assert!(!intents.contains_key(&1), "seat 1 left → forfeited, not in intents");
        assert_eq!(active, active_seats(1), "seat 1 departed the active set");

        // The next tick awaits only seat 0: its buffered line returns at once and the read never
        // blocks on the departed seat 1 (which, still awaited, would cost the full 50 ms budget).
        tx.send(act_line(0, m.match_id(), 0)).unwrap();
        let start = Instant::now();
        let next = read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(50), &mut misses);
        assert_eq!(next.keys().copied().collect::<Vec<_>>(), vec![0], "only the surviving seat is read");
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "the departed seat is not awaited — the read does not wait the budget for it"
        );
        drop(tx);
    }

    #[test]
    fn pump_to_end_deadlined_forfeits_a_leaver_and_reaches_a_bounded_end() {
        // The enforced-path twin of pump_to_end_forfeits_a_leaver...: seat 1 Leaves at tick 0.
        // The Leave is a durable FORFEIT here too — the caller eliminates each seat that departed
        // `active` — so seat 1 is downed and this 1v1 ENDS at tick 0. No hang, and no per-tick
        // timeout spent awaiting the leaver (every line is pre-buffered, so nothing times out).
        let mut m = build_direct_match(&direct_args(2, "", 0), 2);
        let mid = m.match_id();
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(act_line(0, mid, 0)).unwrap();
        tx.send(leave_line(1)).unwrap();
        for t in 1..4 {
            tx.send(act_line(0, mid, t)).unwrap(); // never read — the match ends at tick 0
        }

        let mut out: Vec<u8> = Vec::new();
        let start = Instant::now();
        let result = pump_to_end_deadlined(&mut m, 2, &rx, &mut out, Duration::from_millis(50));
        assert_eq!(m.phase(), MatchPhase::Ended, "the leaver's elimination ends the enforced pump");
        assert_eq!(result.final_tick, 1, "the 1v1 ends at the tick-0 forfeit, not the max_ticks cap");
        let replay = m.into_replay();
        assert_eq!(tick_seats(&replay), vec![(0, vec![0])], "seat 0 acts at tick 0; seat 1 forfeited");
        assert_eq!(tick_forfeits(&replay), vec![(0, vec![1])], "seat 1 forfeits at its tick-0 Leave");
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "the aligned, pre-buffered stream never waits a per-tick budget for the departed seat"
        );
        drop(tx);
    }

    #[test]
    fn read_tick_deadlined_escalates_a_persistently_silent_seat_after_the_threshold() {
        // The escalation core, driven deterministically by a never-sent line (no sleep race — the
        // assertion is on the miss COUNT, not a duration). Seat 0 answers each read; seat 1 is
        // withheld, so every read times out on it and advances its consecutive-miss streak. Seat 1
        // keeps its seat through the sub-threshold misses and is ELIMINATED (departs `active`, which
        // is what the pump turns into m.forfeit) on exactly the MISS_FORFEIT_THRESHOLD-th miss.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        let mut active = active_seats(2);
        let mut misses = BTreeMap::new();
        let (tx, rx) = mpsc::channel::<String>();

        for streak in 1..MISS_FORFEIT_THRESHOLD {
            tx.send(act_line(0, m.match_id(), 0)).unwrap(); // seat 0 answers; seat 1 withheld
            read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(5), &mut misses);
            assert!(active.contains(&1), "a sub-threshold silence keeps seat 1 in the match");
            assert_eq!(misses.get(&1), Some(&streak), "seat 1's consecutive-miss streak tracks each miss");
        }

        tx.send(act_line(0, m.match_id(), 0)).unwrap();
        read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(5), &mut misses);
        assert_eq!(active, active_seats(1), "the threshold-th consecutive miss eliminates seat 1, leaving only seat 0");
        assert_eq!(misses.get(&1), None, "an eliminated seat's streak entry is cleared");
        assert!(!misses.contains_key(&0), "the seat that answered every read never accrued a miss");
        drop(tx);
    }

    #[test]
    fn read_tick_deadlined_resets_the_miss_streak_when_a_seat_answers() {
        // FM3: only a CONSECUTIVE silence eliminates. Seat 1 misses to one shy of the threshold,
        // then ANSWERS — clearing its streak — so a later miss restarts at 1 and it is never
        // eliminated, even though its TOTAL misses now exceed the threshold.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        let mut active = active_seats(2);
        let mut misses = BTreeMap::new();
        let (tx, rx) = mpsc::channel::<String>();

        for _ in 1..MISS_FORFEIT_THRESHOLD {
            tx.send(act_line(0, m.match_id(), 0)).unwrap(); // seat 1 withheld
            read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(5), &mut misses);
        }
        assert_eq!(misses.get(&1), Some(&(MISS_FORFEIT_THRESHOLD - 1)), "seat 1 is one miss from the threshold");

        // Seat 1 answers this read: both seats deliver a line, so the read returns with no timeout.
        tx.send(act_line(0, m.match_id(), 0)).unwrap();
        tx.send(act_line(1, m.match_id(), 0)).unwrap();
        read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(5), &mut misses);
        assert_eq!(misses.get(&1), None, "answering clears seat 1's consecutive-miss streak");
        assert!(active.contains(&1), "seat 1 is still in the match after answering");

        // A fresh miss restarts the streak at 1: despite MORE total misses than the threshold,
        // seat 1 survives, because the run was broken.
        tx.send(act_line(0, m.match_id(), 0)).unwrap();
        read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(5), &mut misses);
        assert_eq!(misses.get(&1), Some(&1), "the post-answer streak restarts at 1, not at the pre-answer count");
        assert!(active.contains(&1), "a non-consecutive silence never eliminates seat 1");
        drop(tx);
    }

    #[test]
    fn read_tick_deadlined_escalates_past_a_departed_seat_stray_flood() {
        // A departed seat flooding strays PAST the deadline must not stall a co-silent seat's
        // escalation. Seat 0 has already left (active = {1}); seat 1 is silent. A zero budget makes
        // the deadline already spent when the buffered stray is read, so each read takes the
        // stray-past-deadline branch deterministically (no flood race) — which must still advance
        // seat 1's streak. Without that, a one-stray-per-tick flooder would peg seat 1 at zero
        // misses forever and strand the match at the cap.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        let mut active: BTreeSet<SeatId> = BTreeSet::from([1]); // seat 0 already departed
        let mut misses = BTreeMap::new();
        let (tx, rx) = mpsc::channel::<String>();

        for streak in 1..MISS_FORFEIT_THRESHOLD {
            tx.send(act_line(0, m.match_id(), 0)).unwrap(); // a stray from the DEPARTED seat 0
            read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(0), &mut misses);
            assert!(active.contains(&1), "the departed-seat stray does not stall seat 1's escalation");
            assert_eq!(misses.get(&1), Some(&streak), "seat 1's streak advances through the stray-past-deadline break");
        }
        tx.send(act_line(0, m.match_id(), 0)).unwrap();
        read_tick_deadlined(&m, &mut active, &rx, Duration::from_millis(0), &mut misses);
        assert!(!active.contains(&1), "seat 1 is eliminated on the threshold-th miss, stray flood notwithstanding");
        drop(tx);
    }

    #[test]
    fn pump_to_end_deadlined_escalates_persistent_silence_to_a_prompt_end() {
        // The whole point, end to end: a match whose seats go persistently silent — the stream
        // stays OPEN (a timeout each tick, NOT an EOF) — must not idle to the max_ticks cap.
        // Nothing is ever sent, so every tick times out and both seats accrue misses; on the
        // MISS_FORFEIT_THRESHOLD-th consecutive miss both are eliminated and the match ends at
        // tick == threshold (3), well before the cap (max_ticks 4). Deterministic: the never-sent
        // lines drive the escalation; the outcome is count-, not duration-, based.
        let mut m = build_direct_match(&direct_args(2, "", 0), 2);
        let (_tx, rx) = mpsc::channel::<String>(); // held open (no EOF) but never fed
        let mut out: Vec<u8> = Vec::new();
        let result = pump_to_end_deadlined(&mut m, 2, &rx, &mut out, Duration::from_millis(5));

        assert_eq!(m.phase(), MatchPhase::Ended, "persistent silence ends the match, it does not idle to the cap");
        assert_eq!(
            result.final_tick as u32, MISS_FORFEIT_THRESHOLD,
            "the match ends on the threshold-th consecutive miss, not at the max_ticks cap"
        );
        assert!(result.outcomes.iter().all(|o| !o.alive_at_end), "both persistently-silent seats are eliminated");
        let replay = m.into_replay();
        assert_eq!(
            tick_forfeits(&replay),
            vec![((MISS_FORFEIT_THRESHOLD - 1) as u64, vec![0, 1])],
            "both seats are durably forfeited together on the escalation tick, and only then"
        );
        assert!(
            tick_seats(&replay).iter().all(|(_, seats)| seats.is_empty()),
            "no seat ever acts — every tick is a silent forfeit"
        );
    }

    #[test]
    fn parse_pickup_radius_maps_a_non_negative_radius() {
        assert_eq!(parse_pickup_radius("0"), 0);
        assert_eq!(parse_pickup_radius("4000"), 4000);
        assert_eq!(
            parse_pickup_radius(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid collection radius"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_pickup_radius_rejects_a_negative() {
        // A collection radius is non-negative; a negative is meaningless (a squared distance is never < 0).
        // Reject it loudly at the CLI; the u32 parse fails the leading '-'.
        parse_pickup_radius("-4000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_pickup_radius_rejects_an_overflow() {
        // A radius past i32::MAX must abort, NOT wrap into a negative. 3_000_000_000 fits a u32 but not an i32,
        // so i32::try_from catches it.
        parse_pickup_radius("3000000000");
    }

    #[test]
    fn pickup_radius_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --pickup-radius pulls exactly one token (through
        // parse_pickup_radius) and consumes no following flag. A --pickup-radius 4000 right before --seats 3
        // must parse BOTH.
        let parsed =
            parse_args_from(["--pickup-radius", "4000", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.pickup_radius, 4000, "--pickup-radius 4000 parses the radius");
        assert_eq!(parsed.seats, 3, "--pickup-radius consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --pickup-radius is NOT 0 — it is the
        // Rules default (a 0 radius is collectible only by a pawn exactly on the pickup, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.pickup_radius,
            Rules::default().pickup_radius,
            "no --pickup-radius defaults to the Rules default radius, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_pickup_radius_into_rules() {
        // FM1 (default drift): no --pickup-radius is the Rules DEFAULT radius — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). Pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().pickup_radius,
            Rules::default().pickup_radius,
            "no --pickup-radius is the Rules default radius (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { pickup_radius: 4000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .pickup_radius,
            4000,
            "--pickup-radius 4000 threads the radius into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_pickup_radius_into_a_matchmade_match() {
        // FM3 (path skew): --pickup-radius must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same radius a
        // hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { pickup_radius: 4000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().pickup_radius,
            4000,
            "the matchmaker forms under --pickup-radius 4000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default radius — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().pickup_radius,
            Rules::default().pickup_radius,
            "no --pickup-radius: the matchmaker forms the Rules default radius"
        );
    }

    #[test]
    fn pickup_respawn_cooldown_parses_as_a_u16_value_flag() {
        // The value-flag twin of the threading tests: --pickup-respawn-cooldown pulls exactly one token and parses
        // it as the u16 dormant-tick count, consuming no following flag. A --pickup-respawn-cooldown 600 right
        // before --seats 3 must parse BOTH.
        let parsed = parse_args_from(
            ["--pickup-respawn-cooldown", "600", "--seats", "3"].into_iter().map(String::from),
        );
        assert_eq!(parsed.pickup_respawn_cooldown, 600, "--pickup-respawn-cooldown 600 parses the cooldown");
        assert_eq!(
            parsed.seats, 3,
            "--pickup-respawn-cooldown consumed exactly one token, so --seats 3 parsed"
        );

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --pickup-respawn-cooldown is NOT 0 — it
        // is the Rules default (a 0 cooldown respawns the pickup the tick after collection, effectively always
        // present, not the pre-flag behaviour).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.pickup_respawn_cooldown,
            Rules::default().pickup_respawn_cooldown,
            "no --pickup-respawn-cooldown defaults to the Rules default cooldown, NOT 0"
        );

        // FM1 (explicit 0 forwards): an explicitly requested 0 is the always-present-pickup degenerate — it must
        // forward verbatim, NOT be coerced back to the default. Only an ABSENT flag is the default.
        let zero = parse_args_from(
            ["--pickup-respawn-cooldown", "0", "--seats", "2"].into_iter().map(String::from),
        );
        assert_eq!(
            zero.pickup_respawn_cooldown, 0,
            "an explicit --pickup-respawn-cooldown 0 forwards (pickup always present)"
        );
    }

    #[test]
    #[should_panic(expected = "u16")]
    fn pickup_respawn_cooldown_rejects_a_negative() {
        // FM2 (type bound): pickup_respawn_cooldown is a u16; a negative is meaningless for a tick count and must
        // abort at the CLI ('-' is not a u16 digit), NOT silently coerce. A value past u16::MAX aborts the same way.
        parse_args_from(["--pickup-respawn-cooldown", "-5"].into_iter().map(String::from));
    }

    #[test]
    fn direct_match_threads_pickup_respawn_cooldown_into_rules() {
        // FM1 (default drift): no --pickup-respawn-cooldown is the Rules DEFAULT cooldown — NOT 0 — byte-identical to
        // the pre-flag harness (and its replay digest). This is the base-balance distinction from the feature-toggle
        // knobs (which default 0/off); pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().pickup_respawn_cooldown,
            Rules::default().pickup_respawn_cooldown,
            "no --pickup-respawn-cooldown is the Rules default cooldown (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(
                &Args { pickup_respawn_cooldown: 600, ..direct_args(2, "reference", 0) },
                2
            )
            .rules()
            .pickup_respawn_cooldown,
            600,
            "--pickup-respawn-cooldown 600 threads the cooldown into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_pickup_respawn_cooldown_into_a_matchmade_match() {
        // FM3 (path skew): --pickup-respawn-cooldown must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same cooldown a
        // hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { pickup_respawn_cooldown: 600, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().pickup_respawn_cooldown,
            600,
            "the matchmaker forms under --pickup-respawn-cooldown 600 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default cooldown — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().pickup_respawn_cooldown,
            Rules::default().pickup_respawn_cooldown,
            "no --pickup-respawn-cooldown: the matchmaker forms the Rules default cooldown"
        );
    }

    #[test]
    fn parse_spawn_jitter_maps_a_non_negative_jitter() {
        assert_eq!(parse_spawn_jitter("0"), 0);
        assert_eq!(parse_spawn_jitter("5000"), 5000);
        assert_eq!(
            parse_spawn_jitter(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid per-axis jitter"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_spawn_jitter_rejects_a_negative() {
        // A spawn jitter is non-negative; a negative would invert the [-jitter, +jitter] draw span (lo > hi).
        // Reject it loudly at the CLI; the u32 parse fails the leading '-'.
        parse_spawn_jitter("-5000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_spawn_jitter_rejects_an_overflow() {
        // A jitter past i32::MAX must abort, NOT wrap into a negative. 3_000_000_000 fits a u32 but not an i32,
        // so i32::try_from catches it.
        parse_spawn_jitter("3000000000");
    }

    #[test]
    fn spawn_jitter_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --spawn-jitter pulls exactly one token (through
        // parse_spawn_jitter) and consumes no following flag. A --spawn-jitter 5000 right before --seats 3
        // must parse BOTH.
        let parsed =
            parse_args_from(["--spawn-jitter", "5000", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.spawn_jitter, 5000, "--spawn-jitter 5000 parses the jitter");
        assert_eq!(parsed.seats, 3, "--spawn-jitter consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --spawn-jitter is NOT 0 — it is the
        // Rules default (a 0 jitter is a fully deterministic opening, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.spawn_jitter,
            Rules::default().spawn_jitter,
            "no --spawn-jitter defaults to the Rules default jitter, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_spawn_jitter_into_rules() {
        // FM1 (default drift): no --spawn-jitter is the Rules DEFAULT jitter — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). Pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().spawn_jitter,
            Rules::default().spawn_jitter,
            "no --spawn-jitter is the Rules default jitter (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { spawn_jitter: 5000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .spawn_jitter,
            5000,
            "--spawn-jitter 5000 threads the jitter into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_spawn_jitter_into_a_matchmade_match() {
        // FM3 (path skew): --spawn-jitter must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same jitter a
        // hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { spawn_jitter: 5000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().spawn_jitter,
            5000,
            "the matchmaker forms under --spawn-jitter 5000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default jitter — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().spawn_jitter,
            Rules::default().spawn_jitter,
            "no --spawn-jitter: the matchmaker forms the Rules default jitter"
        );
    }

    #[test]
    fn parse_spawn_radius_maps_a_non_negative_radius() {
        assert_eq!(parse_spawn_radius("0"), 0);
        assert_eq!(parse_spawn_radius("50000"), 50000);
        assert_eq!(
            parse_spawn_radius(&i32::MAX.to_string()),
            i32::MAX,
            "the whole non-negative i32 range is a valid spawn-line half-width"
        );
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn parse_spawn_radius_rejects_a_negative() {
        // A spawn-line half-width is non-negative; a negative would invert the [-radius, +radius] spread span.
        // Reject it loudly at the CLI; the u32 parse fails the leading '-'.
        parse_spawn_radius("-50000");
    }

    #[test]
    #[should_panic(expected = "i32 range")]
    fn parse_spawn_radius_rejects_an_overflow() {
        // A half-width past i32::MAX must abort, NOT wrap into a negative. 3_000_000_000 fits a u32 but not an i32,
        // so i32::try_from catches it.
        parse_spawn_radius("3000000000");
    }

    #[test]
    fn spawn_radius_parses_as_a_value_flag() {
        // The value-flag twin of the threading tests: --spawn-radius pulls exactly one token (through
        // parse_spawn_radius) and consumes no following flag. A --spawn-radius 50000 right before --seats 3
        // must parse BOTH.
        let parsed =
            parse_args_from(["--spawn-radius", "50000", "--seats", "3"].into_iter().map(String::from));
        assert_eq!(parsed.spawn_radius, 50000, "--spawn-radius 50000 parses the half-width");
        assert_eq!(parsed.seats, 3, "--spawn-radius consumed exactly one token, so --seats 3 parsed");

        // FM1 (non-zero default): UNLIKE the feature-toggle knobs, an absent --spawn-radius is NOT 0 — it is the
        // Rules default (a 0 radius stacks every seat on the X origin, not the pre-flag harness).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert_eq!(
            none.spawn_radius,
            Rules::default().spawn_radius,
            "no --spawn-radius defaults to the Rules default half-width, NOT 0"
        );
    }

    #[test]
    fn direct_match_threads_spawn_radius_into_rules() {
        // FM1 (default drift): no --spawn-radius is the Rules DEFAULT half-width — NOT 0 — byte-identical to the
        // pre-flag harness (and its replay digest). Pin the default reproduces, not zeroes.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().spawn_radius,
            Rules::default().spawn_radius,
            "no --spawn-radius is the Rules default half-width (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { spawn_radius: 50000, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .spawn_radius,
            50000,
            "--spawn-radius 50000 threads the half-width into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_spawn_radius_into_a_matchmade_match() {
        // FM3 (path skew): --spawn-radius must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under the same half-width
        // a hand-seated one does (read back via the same rules() accessor).
        let mm = build_matchmaker(&Args { spawn_radius: 50000, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().spawn_radius,
            50000,
            "the matchmaker forms under --spawn-radius 50000 (matchmade == hand-seated)"
        );

        // No flag still forms the Rules default half-width — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().spawn_radius,
            Rules::default().spawn_radius,
            "no --spawn-radius: the matchmaker forms the Rules default half-width"
        );
    }

    #[test]
    fn direct_match_threads_the_weapon_mode_into_rules() {
        // FM1 (default drift): no --weapon-mode is Hitscan — the instant beam, byte-identical to
        // the pre-flag harness (and its replay digest). The fire BEHAVIOR (a projectile flies, a
        // melee cleaves) is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().weapon_mode,
            WeaponMode::Hitscan,
            "no --weapon-mode is Hitscan (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(
                &Args { weapon_mode: WeaponMode::Projectile, ..direct_args(2, "reference", 0) },
                2
            )
            .rules()
            .weapon_mode,
            WeaponMode::Projectile,
            "--weapon-mode projectile threads Projectile into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_the_weapon_mode_into_a_matchmade_match() {
        // FM3 (path skew): --weapon-mode must reach the --mode path too, not just the direct one.
        // build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms under it —
        // proven by forming a 2-seat Human match and reading rules() back (the same accessor the
        // direct twin uses, so matchmade and hand-seated agree on the weapon).
        let mm =
            build_matchmaker(&Args { weapon_mode: WeaponMode::Projectile, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().weapon_mode,
            WeaponMode::Projectile,
            "the matchmaker forms under --weapon-mode projectile (matchmade == hand-seated)"
        );

        // No flag still forms Hitscan — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(
            off.rules().weapon_mode,
            WeaponMode::Hitscan,
            "no --weapon-mode: the matchmaker forms Hitscan"
        );
    }

    #[test]
    fn matchmade_named_arena_reaches_the_start_frame() {
        // FM4: --map reference reaches the MATCHMADE path too, and the geometry crosses the
        // wire — the Start frame an agent receives carries the cover + pickups, not just the
        // in-memory Match. Two agents sign their seat's challenge and form an Agent match.
        let sk0 = join_key();
        let sk1 = other_join_key();
        let addr0 = address_from_verifying_key(sk0.verifying_key());
        let addr1 = address_from_verifying_key(sk1.verifying_key());
        let sig0 = sign_join_proof(&sk0, &addr0, nonce_for(id(), 0).as_bytes());
        let sig1 = sign_join_proof(&sk1, &addr1, nonce_for(id(), 1).as_bytes());
        let input = format!("{}\n{}\n", join_line(0, &addr0, &sig0), join_line(1, &addr1, &sig1));
        let mut lines = io::BufReader::new(io::Cursor::new(input)).lines();
        let mut out: Vec<u8> = Vec::new();
        let mut args = mode_args(2, MatchMode::Agent, vec![]);
        args.arena = "reference";

        let (_mm, m) = handshake_matchmade(&args, MatchMode::Agent, 2, &None, &mut lines, &mut out);
        assert!(!m.blockers().is_empty(), "the formed match plays under the reference arena's cover");
        assert_eq!(m.pickup_spawns().len(), 2, "and its two health pickups");

        let stdout = String::from_utf8(out).unwrap();
        let GatewayMsg::Start { blockers, pickup_points, .. } = first_start(&stdout) else {
            unreachable!("first_start returns a Start variant")
        };
        assert!(!blockers.is_empty(), "the agent's Start frame carries the cover");
        assert_eq!(pickup_points.len(), 2, "the agent's Start frame carries the two pickup points");
    }

    #[test]
    fn join_request_for_infers_controller_kind_from_mode_and_signature() {
        // Human mode: a token-less seat is a human; a SIGNED join is an agent intruder.
        assert_eq!(join_request_for(MatchMode::Human, 0, &[], "h", "").kind, ControllerKind::Human);
        let intruder = join_request_for(MatchMode::Human, 0, &[], "a", "deadbeef");
        assert_eq!(intruder.kind, ControllerKind::Agent, "a signed join in human mode is the agent intruder");
        // Agent mode: every seat is an agent — ranked iff a token is present.
        let casual = join_request_for(MatchMode::Agent, 0, &[], "a", "");
        assert_eq!((casual.kind, casual.token), (ControllerKind::Agent, None));
        let ranked = join_request_for(MatchMode::Agent, 0, &[], "a", "ff");
        assert_eq!(ranked.kind, ControllerKind::Agent);
        assert_eq!(ranked.token.as_deref(), Some("ff"));
        // Mixed: a declared human seat is human; any other is an agent (casual if token-less).
        assert_eq!(join_request_for(MatchMode::Mixed, 0, &[0], "h", "").kind, ControllerKind::Human);
        let casual_mixed = join_request_for(MatchMode::Mixed, 1, &[0], "a", "");
        assert_eq!((casual_mixed.kind, casual_mixed.token), (ControllerKind::Agent, None));
    }

    #[test]
    fn agent_mode_forms_an_authenticated_match_and_settles_to_the_verified_addresses() {
        // FM1: the harness pumps the matchmaker's FORMED match — its own minted id, its
        // verified-address roster IN SEAT ORDER — not a self-built Match on the challenge
        // salt with agent-{i} labels. Two agents sign their seat's challenge;
        // handshake_matchmade routes both through the Matchmaker and returns the formed
        // match, which then pumps + settles.
        let sk0 = join_key();
        let sk1 = other_join_key();
        let addr0 = address_from_verifying_key(sk0.verifying_key());
        let addr1 = address_from_verifying_key(sk1.verifying_key());
        let sig0 = sign_join_proof(&sk0, &addr0, nonce_for(id(), 0).as_bytes());
        let sig1 = sign_join_proof(&sk1, &addr1, nonce_for(id(), 1).as_bytes());
        let input = format!("{}\n{}\n", join_line(0, &addr0, &sig0), join_line(1, &addr1, &sig1));
        let mut lines = io::BufReader::new(io::Cursor::new(input)).lines();
        let mut out: Vec<u8> = Vec::new();
        let args = mode_args(2, MatchMode::Agent, vec![]);

        let (_mm, mut m) = handshake_matchmade(&args, MatchMode::Agent, 2, &None, &mut lines, &mut out);
        let minted = m.match_id();
        assert_ne!(minted, id(), "the formed match carries the matchmaker's minted id, not the challenge salt");
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains(&minted.to_string()), "welcome/start announce the formed match id");

        while m.phase() == MatchPhase::Live {
            m.step(&BTreeMap::new());
        }
        let result = m.result().expect("ended").clone();
        let replay = m.into_replay();
        let controllers: Vec<&str> = replay.seats.iter().map(|s| s.controller.as_str()).collect();
        assert_eq!(
            controllers,
            vec![addr0.as_str(), addr1.as_str()],
            "the formed roster credits the verified addresses in seat order (seat i = the seat-i signer)",
        );

        let settler = MockSettler::default();
        settle_match(&settler, &result, &replay, None).expect("a 2-seat match settles");
        match settler.resolution(minted).expect("resolved") {
            Resolution::Win { winner, .. } => {
                assert!(winner == addr0 || winner == addr1, "the winner is a verified address: {winner}")
            }
            Resolution::Draw { .. } => {}
            other => panic!("a played 1v1 settles Win or Draw, got {other:?}"),
        }
    }

    #[test]
    fn the_ranked_join_is_verified_against_the_seat_challenge_not_the_formed_id() {
        // FM2: the matchmaker checks the signature against the per-connection CHALLENGE
        // nonce passed to join() — the harness must hand it exactly the nonce it issued,
        // never the id the matchmaker mints after admission. A signature over seat 0's
        // nonce, presented under seat 1's, recovers a different address and is refused.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let sig_over_seat0 = sign_join_proof(&sk, &addr, nonce_for(id(), 0).as_bytes());

        let mm = mm2();
        let matched = join_request_for(MatchMode::Agent, 0, &[], &addr, &sig_over_seat0);
        assert!(
            matches!(mm.join(MatchMode::Agent, nonce_for(id(), 0).as_bytes(), matched), Ok(JoinOutcome::Queued)),
            "the signature over this seat's challenge is admitted",
        );
        let mismatched = join_request_for(MatchMode::Agent, 1, &[], &addr, &sig_over_seat0);
        assert!(
            matches!(
                mm.join(MatchMode::Agent, nonce_for(id(), 1).as_bytes(), mismatched),
                Err(JoinError::Unauthenticated { .. })
            ),
            "the same signature under a different challenge nonce is refused",
        );
    }

    #[test]
    fn human_mode_refuses_a_signed_agent_join() {
        // FM3: a signed join in human-only mode is an agent presenting a ranked claim —
        // refused WrongKindForMode, never seated, so a human match stays human.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let sig = sign_join_proof(&sk, &addr, nonce_for(id(), 0).as_bytes());
        let req = join_request_for(MatchMode::Human, 0, &[], &addr, &sig);
        assert!(matches!(
            mm2().join(MatchMode::Human, nonce_for(id(), 0).as_bytes(), req),
            Err(JoinError::WrongKindForMode { kind: ControllerKind::Agent, mode: MatchMode::Human }),
        ));
    }

    #[test]
    fn agent_mode_refuses_a_token_less_join() {
        // FM3: Agent mode is ranked-only — a token-less agent is unauthenticated and
        // never reaches a ranked seat.
        let req = join_request_for(MatchMode::Agent, 0, &[], "0xnobody", "");
        assert!(matches!(
            mm2().join(MatchMode::Agent, nonce_for(id(), 0).as_bytes(), req),
            Err(JoinError::Unauthenticated { .. }),
        ));
    }

    #[test]
    fn mixed_mode_admits_a_token_less_agent_as_casual_and_forms_with_a_human() {
        // FM3: a token-less agent in Mixed is admitted as a casual cross-play seat (not
        // rejected), and a human + that casual agent forms a Mixed match.
        let mm = mm2();
        let human = join_request_for(MatchMode::Mixed, 0, &[0], "human-0", "");
        assert!(
            matches!(mm.join(MatchMode::Mixed, nonce_for(id(), 0).as_bytes(), human), Ok(JoinOutcome::Queued)),
            "the human seat queues",
        );
        let casual = join_request_for(MatchMode::Mixed, 1, &[0], "agent-1", "");
        let formed = mm.join(MatchMode::Mixed, nonce_for(id(), 1).as_bytes(), casual).expect("admitted casual");
        assert!(formed.into_formed().is_some(), "a human + a casual agent forms a Mixed cross-play match");
    }

    #[test]
    fn matchmade_ranked_admission_enforces_the_registered_set() {
        // End to end through build_matchmaker: --registered is the eligibility set the
        // matchmaker gates ranked admission on. A registered key holder is admitted; a key
        // holder that signs correctly but is NOT registered is refused — the match it would
        // form could never settle on-chain (MatchSettlement AgentNotRegistered).
        let registered_sk = join_key();
        let outsider_sk = other_join_key();
        let registered_addr = address_from_verifying_key(registered_sk.verifying_key());
        let outsider_addr = address_from_verifying_key(outsider_sk.verifying_key());

        // Only the registered agent is listed as eligible.
        let args = Args { registered: vec![registered_addr.clone()], ..direct_args(2, "", 0) };
        let mm = build_matchmaker(&args, 2);

        let nonce0 = nonce_for(id(), 0);
        let reg_req = join_request_for(
            MatchMode::Agent,
            0,
            &[],
            &registered_addr,
            &sign_join_proof(&registered_sk, &registered_addr, nonce0.as_bytes()),
        );
        assert!(
            matches!(mm.join(MatchMode::Agent, nonce0.as_bytes(), reg_req), Ok(JoinOutcome::Queued)),
            "a registered key holder is admitted to a ranked seat",
        );

        let nonce1 = nonce_for(id(), 1);
        let out_req = join_request_for(
            MatchMode::Agent,
            1,
            &[],
            &outsider_addr,
            &sign_join_proof(&outsider_sk, &outsider_addr, nonce1.as_bytes()),
        );
        assert!(
            matches!(
                mm.join(MatchMode::Agent, nonce1.as_bytes(), out_req),
                Err(JoinError::NotRegistered { .. })
            ),
            "an unregistered key holder is refused even with a valid signature over its challenge",
        );
        assert_eq!(mm.waiting(MatchMode::Agent), 1, "only the registered seat queued; the outsider never entered");
    }

    #[test]
    fn registered_flag_does_not_gate_mixed_cross_play() {
        // Regression guard: --registered gates ranked (Agent) admission only. A signed agent
        // that is NOT registered must still join a Mixed casual match (which never settles),
        // rather than have its honest signature cancel the whole match.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        // Register some OTHER agent, not this one.
        let other = address_from_verifying_key(other_join_key().verifying_key());
        let args = Args { registered: vec![other], ..direct_args(2, "", 0) };
        let mm = build_matchmaker(&args, 2);
        let nonce0 = nonce_for(id(), 0);
        let req = join_request_for(MatchMode::Mixed, 0, &[], &addr, &sign_join_proof(&sk, &addr, nonce0.as_bytes()));
        assert!(
            matches!(mm.join(MatchMode::Mixed, nonce0.as_bytes(), req), Ok(JoinOutcome::Queued)),
            "a signed unregistered agent joins Mixed cross-play on possession — registration gates ranked only",
        );
    }

    #[test]
    fn no_registered_flag_leaves_ranked_admission_possession_only() {
        // The default (no --registered) must not gate on registration — byte-identical to the
        // possession-only ranked path: a signed key holder is admitted with no eligibility set.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let mm = build_matchmaker(&direct_args(2, "", 0), 2);
        let nonce0 = nonce_for(id(), 0);
        let req = join_request_for(
            MatchMode::Agent,
            0,
            &[],
            &addr,
            &sign_join_proof(&sk, &addr, nonce0.as_bytes()),
        );
        assert!(
            matches!(mm.join(MatchMode::Agent, nonce0.as_bytes(), req), Ok(JoinOutcome::Queued)),
            "with no --registered set, a signed seat is admitted (possession-only, unchanged)",
        );
    }

    /// Form a ranked Agent match of `keys.len()` seats through a fresh matchmaker — each
    /// seat signs its challenge so the verifier admits it. Returns the matchmaker (its
    /// ladder + the pending_ranked registration live) and the formed match, so a test can
    /// settle the match's terminal result back into the ladder.
    fn formed_ranked_match(keys: &[SigningKey]) -> (Matchmaker<SignatureVerifier>, Match) {
        let mm = Matchmaker::new(SignatureVerifier, matchmaker_params(keys.len() as u8, 4, ""));
        let mut formed = None;
        for (seat, sk) in keys.iter().enumerate() {
            let seat = seat as SeatId;
            let addr = address_from_verifying_key(sk.verifying_key());
            let nonce = nonce_for(id(), seat);
            let req = join_request_for(MatchMode::Agent, seat, &[], &addr, &sign_join_proof(sk, &addr, nonce.as_bytes()));
            if let Some(m) = mm.join(MatchMode::Agent, nonce.as_bytes(), req).expect("admitted").into_formed() {
                formed = Some(m);
            }
        }
        (mm, formed.expect("the last seat forms the match"))
    }

    /// A synthetic terminal result for `match_id` with the given placement `outcomes`.
    /// The id is the matchmaker's MINTED id (not the fixed test id), so the ladder settle
    /// resolves the registration `build()` keyed under it.
    fn ranked_result(match_id: Uuid, outcomes: Vec<SeatOutcome>) -> MatchResult {
        MatchResult { protocol_version: PROTOCOL_VERSION, match_id, final_tick: 1, outcomes, replay_hash: "00".repeat(32) }
    }

    #[test]
    fn settle_ranked_ladder_moves_a_1v1_winner_up_and_loser_down_by_the_configured_k() {
        // A formed Agent 1v1 settled into the ladder: seat 0 wins, so its agent gains and
        // seat 1's loses by the EXACT zero-sum ranked_delta the core computes at DEV_MOCK_K
        // from the two DEFAULT_RATING pre-ratings, and the pending_ranked entry is consumed.
        // A bare-literal K would move a different magnitude; an unmoved ladder or a still
        // -pending entry would mean the result never settled.
        let keys = [join_key(), other_join_key()];
        let addr0 = address_from_verifying_key(keys[0].verifying_key());
        let addr1 = address_from_verifying_key(keys[1].verifying_key());
        let (mm, m) = formed_ranked_match(&keys);
        assert_eq!(mm.unsettled_ranked(), 1, "the formed Agent match registered one pending result");

        let result = ranked_result(m.match_id(), vec![outcome(0, 1, 10, true), outcome(1, 2, 0, false)]);
        let expected = ranked_delta(&result, DEFAULT_RATING, DEFAULT_RATING, DEV_MOCK_K).unwrap();
        settle_ranked_ladder(&mm, &result, m.seats());

        assert!(expected.a > 0 && expected.a == -expected.b, "a decisive win is a positive, zero-sum move");
        assert_eq!(mm.rating(&addr0), Some(DEFAULT_RATING + expected.a), "winner moves by +delta at the configured K");
        assert_eq!(mm.rating(&addr1), Some(DEFAULT_RATING + expected.b), "loser moves by -delta");
        assert_eq!(mm.unsettled_ranked(), 0, "the registration is consumed");
    }

    #[test]
    fn settle_ranked_ladder_is_idempotent_on_a_replayed_result() {
        // FM2: applying the same terminal result twice (a retry / duplicate End) must not
        // move ratings twice — the registration is consumed on the first settle, so the
        // second is a no-op.
        let keys = [join_key(), other_join_key()];
        let addr0 = address_from_verifying_key(keys[0].verifying_key());
        let (mm, m) = formed_ranked_match(&keys);
        let result = ranked_result(m.match_id(), vec![outcome(0, 1, 10, true), outcome(1, 2, 0, false)]);

        settle_ranked_ladder(&mm, &result, m.seats());
        let after_first = mm.rating(&addr0);
        assert_eq!(mm.unsettled_ranked(), 0);
        settle_ranked_ladder(&mm, &result, m.seats());
        assert_eq!(mm.rating(&addr0), after_first, "a replayed result does not move the ladder again");
        assert_eq!(mm.unsettled_ranked(), 0, "still consumed, not re-registered");
    }

    #[test]
    fn settle_ranked_ladder_settles_a_3_seat_field_through_the_field_arm() {
        // FM1: a 3-seat result MUST settle through apply_ranked_field_result, moving every
        // seat by its placement delta and consuming the registration. Routed (wrongly)
        // through the 1v1 arm it is a silent no-op — ladder unmoved, registration leaked —
        // so a moved 3-seat ladder proves the arm is chosen by outcome count.
        let keys = [join_key(), other_join_key(), third_join_key()];
        let addrs: Vec<String> = keys.iter().map(|k| address_from_verifying_key(k.verifying_key())).collect();
        let (mm, m) = formed_ranked_match(&keys);
        assert_eq!(mm.unsettled_ranked(), 1);

        let result = ranked_result(
            m.match_id(),
            vec![outcome(0, 1, 9, true), outcome(1, 2, 5, true), outcome(2, 3, 1, false)],
        );
        let expected = ranked_field_delta(&result, &[DEFAULT_RATING; 3], DEV_MOCK_K).unwrap();
        settle_ranked_ladder(&mm, &result, m.seats());

        assert!(expected[0].delta > 0 && expected[2].delta < 0, "1st gains, last loses");
        assert_eq!(expected.iter().map(|d| i64::from(d.delta)).sum::<i64>(), 0, "the field is zero-sum");
        for (i, d) in expected.iter().enumerate() {
            assert_eq!(mm.rating(&addrs[i]), Some(DEFAULT_RATING + d.delta), "seat {i} moves by its field delta");
        }
        assert_eq!(mm.unsettled_ranked(), 0, "the field registration is consumed");
    }

    /// A process-unique temp path per test (tests run in parallel threads), tagged so two
    /// ladder tests never share a file.
    fn temp_ladder_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("arena-ladder-test-{}-{tag}.json", std::process::id()))
    }

    #[test]
    fn a_ladder_file_persists_ratings_across_two_sequential_runs() {
        // The headline: run 1 moves the ladder and writes the file; run 2 SEEDS from it, so
        // the moved standings survive a fresh process instead of resetting to DEFAULT_RATING.
        let path = temp_ladder_path("persist");
        let _ = std::fs::remove_file(&path);

        // Run 1: form + settle a ranked 1v1, then persist the moved ladder.
        let keys = [join_key(), other_join_key()];
        let addr0 = address_from_verifying_key(keys[0].verifying_key());
        let addr1 = address_from_verifying_key(keys[1].verifying_key());
        let (mm1, m) = formed_ranked_match(&keys);
        let result = ranked_result(m.match_id(), vec![outcome(0, 1, 10, true), outcome(1, 2, 0, false)]);
        settle_ranked_ladder(&mm1, &result, m.seats());
        let moved0 = mm1.rating(&addr0).expect("seat 0 has a moved rating");
        let moved1 = mm1.rating(&addr1).expect("seat 1 has a moved rating");
        assert_ne!(moved0, DEFAULT_RATING, "the winner actually moved off the default");
        write_ladder(&path, &mm1.snapshot()).expect("persist the moved ladder");

        // Run 2: a fresh matchmaker built from the same --ladder-file resumes those ratings.
        let mut args = mode_args(2, MatchMode::Agent, vec![]);
        args.ladder_file = Some(path.clone());
        let mm2 = build_matchmaker(&args, 2);
        assert_eq!(mm2.rating(&addr0), Some(moved0), "run 2 resumes the winner's standing exactly");
        assert_eq!(mm2.rating(&addr1), Some(moved1), "run 2 resumes the loser's standing exactly");
        assert_eq!(mm2.unsettled_ranked(), 0, "a restore starts with no pending registrations");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_ladder_file_starts_a_fresh_ladder_identical_to_no_file() {
        // A --ladder-file that does not exist is the legal "start fresh" path: the built
        // matchmaker is byte-identical to one with no flag (an empty DEFAULT_RATING ladder),
        // so a first run against a not-yet-written file behaves exactly like today.
        let path = temp_ladder_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(read_ladder_file(&path).expect("a missing file is not an error").is_none(), "missing reads as start-fresh");

        let mut args = mode_args(2, MatchMode::Agent, vec![]);
        args.ladder_file = Some(path);
        let from_missing = build_matchmaker(&args, 2);
        let no_file = Matchmaker::new(SignatureVerifier, matchmaker_params(2, 4, ""));
        assert_eq!(from_missing.snapshot(), no_file.snapshot(), "a missing file yields the fresh in-memory ladder");
    }

    #[test]
    fn an_empty_ladder_file_starts_fresh_not_an_error() {
        // A 0-byte (or whitespace-only) file — e.g. a freshly `touch`ed path — is also the
        // start-fresh signal, distinct from a present snapshot, so it never errors.
        let path = temp_ladder_path("empty");
        std::fs::write(&path, b"   \n").expect("write an empty file");
        assert!(read_ladder_file(&path).expect("an empty file is not an error").is_none(), "empty reads as start-fresh");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_corrupt_or_stale_schema_ladder_file_is_a_loud_error_not_a_silent_reset() {
        // FM2: a present, non-empty file the harness can't trust must surface an Err (the run
        // aborts), NEVER a silent fresh ladder that would erase real standings.
        let path = temp_ladder_path("corrupt");

        // Non-JSON garbage: a hard parse error, not Ok(None).
        std::fs::write(&path, b"not a snapshot {{{").expect("write garbage");
        assert!(matches!(read_ladder_file(&path), Err(LadderFileError::Parse(_))), "garbage is a loud parse Err");

        // Valid JSON but a stale schema version: read parses it, but from_snapshot rejects it,
        // so build_matchmaker would abort rather than restore wrong ratings.
        let stale = LadderSnapshot {
            version: arena_match::LADDER_SNAPSHOT_VERSION + 1,
            ratings: BTreeMap::from([("0xabc".to_string(), 1800)]),
        };
        write_ladder(&path, &stale).expect("write a stale-schema snapshot");
        let parsed = read_ladder_file(&path).expect("valid JSON parses").expect("non-empty file");
        assert!(
            matches!(
                Matchmaker::from_snapshot(SignatureVerifier, matchmaker_params(2, 4, ""), parsed),
                Err(SnapshotError::Version { .. })
            ),
            "a stale-schema snapshot is rejected on restore, not silently loaded",
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_ladder_stages_through_a_temp_and_never_corrupts_the_prior_snapshot() {
        // FM3: the write stages to a sibling temp then atomic-renames, so an interrupted
        // persist (one that never reached the rename) leaves the PRIOR good snapshot intact.
        let path = temp_ladder_path("atomic");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(ladder_tmp_path(&path));

        let a = LadderSnapshot { version: arena_match::LADDER_SNAPSHOT_VERSION, ratings: BTreeMap::from([("0xa".to_string(), 1700)]) };
        write_ladder(&path, &a).expect("write A");
        assert!(!ladder_tmp_path(&path).exists(), "the staging temp is renamed away, never left behind");
        assert_eq!(read_ladder_file(&path).unwrap(), Some(a.clone()), "the live file reads back as A");
        assert_eq!(ladder_tmp_path(&path).parent(), path.parent(), "the temp is a same-directory sibling (atomic rename)");

        // A garbage half-write to the temp path (a persist interrupted before the rename) must
        // NOT touch the live file: it still reads as the prior good A.
        std::fs::write(ladder_tmp_path(&path), b"half written {").expect("stage garbage on the temp");
        assert_eq!(read_ladder_file(&path).unwrap(), Some(a), "an unfinished temp leaves the prior snapshot intact");

        // A completed overwrite swaps the live file to B atomically (consuming the temp).
        let b = LadderSnapshot { version: arena_match::LADDER_SNAPSHOT_VERSION, ratings: BTreeMap::from([("0xb".to_string(), 1500)]) };
        write_ladder(&path, &b).expect("write B");
        assert_eq!(read_ladder_file(&path).unwrap(), Some(b), "the live file is now B, never a half-write");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(ladder_tmp_path(&path));
    }
}
