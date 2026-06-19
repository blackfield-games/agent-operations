"""A deterministic, integer-only scripted arena agent — the reference opponent.

Given a parity-bounded Observation it returns a legal ActionIntent with no float
and no randomness, so a baseline-vs-baseline match is fully determined by the
match seed. It closes on the nearest visible enemy, aims to the octant the core
resolves hits against (so a coarse 8-way aim still lands), and fires while it has
ammo; with no enemy in sight it advances on the arena centre to find one, and
reloads when empty.

The aim is the octant whose Q12 unit vector best points at the target — the same
eight octants arena-core snaps facing to — picked by integer dot product, so the
baseline plays to the reference combat model exactly and stays replay-stable.
"""

from __future__ import annotations

from .proto import ActionButtons, ActionIntent, Observation, Vec2, VisibleEntity

_OCTANT_SCALE = 4096
_OCTANT_DIAG = 2896
_OCTANTS = [
    (_OCTANT_SCALE, 0),            # E
    (_OCTANT_DIAG, _OCTANT_DIAG),  # NE
    (0, _OCTANT_SCALE),            # N
    (-_OCTANT_DIAG, _OCTANT_DIAG),  # NW
    (-_OCTANT_SCALE, 0),           # W
    (-_OCTANT_DIAG, -_OCTANT_DIAG),  # SW
    (0, -_OCTANT_SCALE),           # S
    (_OCTANT_DIAG, -_OCTANT_DIAG),  # SE
]
_BAM_STEP = 8192  # a full turn (65536) / 8 octants


def aim_at(dx: int, dy: int) -> int:
    """The BAM facing whose octant points most directly at (dx, dy): argmax of the
    integer dot product with each octant unit vector, lowest index winning ties.
    `aim = k * _BAM_STEP` lands exactly on octant k, which the core's
    `octant_unit()` snaps straight back to that octant. (0, 0) -> 0 (a stable
    default east)."""
    best_k = 0
    best_dot: int | None = None
    for k, (ux, uy) in enumerate(_OCTANTS):
        dot = dx * ux + dy * uy
        if best_dot is None or dot > best_dot:
            best_dot = dot
            best_k = k
    return best_k * _BAM_STEP


def _toward(dx: int, dy: int, fire: bool) -> ActionIntent:
    return ActionIntent(
        move_dir=Vec2(x=dx, y=dy),
        aim=aim_at(dx, dy),
        buttons=ActionButtons(fire=fire, jump=False, ability=False, reload=False),
    ).clamped()


def _hold(facing: int, reload: bool = False) -> ActionIntent:
    return ActionIntent(
        move_dir=Vec2(x=0, y=0),
        aim=facing,
        buttons=ActionButtons(fire=False, jump=False, ability=False, reload=reload),
    )


class BaselinePolicy:
    """A reusable Policy (Observation -> ActionIntent). Stateless and deterministic:
    the decision depends only on the observation, so one instance can serve a seat
    or be shared across seats."""

    def __call__(self, obs: Observation) -> ActionIntent:
        me = obs.own
        if not me.alive:
            return _hold(me.facing)
        if me.ammo == 0:
            return _hold(me.facing, reload=True)
        target = self._nearest_enemy(obs)
        if target is None:
            # No enemy perceived: close on the arena centre to find one. Holding
            # would risk a scoreless stalemate when seats spawn out of sight.
            cx, cy = -me.position.x, -me.position.y
            if cx == 0 and cy == 0:
                return _hold(me.facing)
            return _toward(cx, cy, fire=False)
        dx = target.position.x - me.position.x
        dy = target.position.y - me.position.y
        return _toward(dx, dy, fire=True)

    @staticmethod
    def _nearest_enemy(obs: Observation) -> VisibleEntity | None:
        me = obs.own
        enemies = [e for e in obs.visible if e.kind == "player" and e.team != me.team]
        if not enemies:
            return None
        return min(
            enemies,
            key=lambda e: (
                (e.position.x - me.position.x) ** 2 + (e.position.y - me.position.y) ** 2,
                e.entity_id,
            ),
        )
