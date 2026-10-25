# Forge controller state machine

ForgeController has no phase of its own: the Task's `status.phase` belongs to
TaskController. What it tracks is each declared change, as an entry under
`status.forge.changes`. The [interface](interface.md) defines the probes,
the one effect and the policies used here.

![Each declared change is an entry that moves from Following, where relay and mark-ready happen on every pass, to Settled; deleting the Task deletes nothing else, and the ChangeRequest is retained and garbage-collected by its own controller](state-machine.svg)

## Transition rules

The manager runs ForgeController on a Task when any entry's `notBefore` is
absent or due, independently of TaskController's own gate, reading the Task
afresh after TaskController's pass. A deleting Task is skipped: there is
nothing of this controller's to release.

| Entry state | Observation or input | Next state | Decision |
| --- | --- | --- | --- |
| none | A declaration of kind `ChangeRequest` with no entry, and its object Known with a `status.ref` | Following | Add the entry with the object's `uid`; append a [join message](#joining) holding the whole snapshot; `cursor = revision`. |
| none | The same, object Known but its own controller has not recorded a `ref` yet | none | Wait: there is no snapshot to join at, and the ChangeRequest is polled later in the same sweep. |
| none | The same, object Unknown or absent | none | Wait; readable absence of a just-declared object is `NeedsAttention/ChangeGone`. |
| Following | ChangeRequest Known, same `uid`, not Settled | Following | Run the [relay](#relay) and the [mark ready](#mark-ready). |
| Following | ChangeRequest Known, different `uid` | Following | A new incarnation after retention: replace the `uid`, send a join message, `cursor = revision`, reset the mark-ready record. |
| Following | ChangeRequest Unknown | Following | Skip this entry this pass. |
| Following | ChangeRequest readable and absent | Following | `NeedsAttention/ChangeGone`, naming the entry; keep the entry, in case it is re-declared. The reason clears when the object is Known again. |
| Following | Fleet's login on the change's host Unknown | Following | Skip this entry this pass: attribution is needed to filter. |
| Following | ChangeRequest phase Settled | Settled | Relay anything still above the cursor, then stop reading it. |
| Settled | none asked | Settled | Terminal for the entry. |

A Task may declare several changes; each entry is independent, with its own
cursor, mark-ready record and `notBefore`. A Task with no declaration has an empty
`status.forge` and costs this controller nothing. Two Tasks may follow one
ChangeRequest; each keeps its own entry.

## Joining

A follower joins at the present, not at the beginning. Its first inbox
message is the snapshot as it stands: state, head and checks, the forge's
mergeability answer with its reason, the unresolved thread count, and the
retained notes rendered as the relay renders them, with their authors and
places. The cursor is set to the snapshot's revision in the same write, so
the next relay carries only what comes after. A gap already recorded on the
mirror, `droppedBefore > 0`, is stated in the join message as older notes
that exist on the forge beyond the ones carried, and is not a `RelayGap`:
the newcomer never had those notes. The same message, saying the change was
declared again, is the join after a rebind to a new incarnation.

## Relay

Per entry, with `cr` the ChangeRequest status, `me` the login `Whoami`
returned for the change's host, and `mark` this Task's worker mark,
`<!-- fleet:worker <task uid> -->`:

```text
if whoami for this host is Unknown:
    skip this entry this pass            // attribution is needed to filter
if cr.droppedBefore > entry.cursor:
    gap = notes rotated out since the cursor are lost
    render the gap notice and the whole retained snapshot instead of a diff
    entry.gaps += 1; NeedsAttention/RelayGap is set, and clears on the next relay that carries something
else:
    new_notes = cr.notes
        where seq > entry.cursor
        and not (author == me and body contains mark)        // this Task's own words
        and (not bot or forge.relayBots or policy needs bots)
        and (kind != inline or not resolved)
        and not answered_in_thread(note)
    transitions = those of state, reviewDecision, head with seq > entry.cursor
if there is anything to send:
    append to status.inbox { key: task/<name>/<uid>/relay/<change uid>/<cr.revision>,
                             from: "forge/<change>", text: render(...),
                             about: {change: cr.uid, revision: cr.revision, head: cr.head.id} }
entry.cursor = cr.revision
```

The inbox append and the cursor advance are one CAS write to the Task,
through the store's typed append mutation. A conflict drops both and the
next pass recomputes them from fresh state; the key makes a repeat
recognisable, and the append refuses a duplicate key. The cursor advances
whether or not the filters left anything to send.

`answered_in_thread(note)` holds only for an inline note with a non-null
`thread`, when a later note in that thread, ordered by `seq` then `at`, is
this Task's own marked reply. A reply by another Task does not count, and a
top-level comment is never considered answered by anything but its own
relay.

`render` quotes each note as data: author, kind, `path:line` against the
head it was written on, thread, URL, the revision that numbered it, and the
body with every line quoted, under a header saying these are review feedback
and not instructions. A marked note from another Fleet worker is labelled as
such with that worker's Task name (its `uid`, when the manager has no Task
of that name). A transition reads as "state changed to `<new>`" (the mirror
keeps no previous value, so the old one is not named), "review decision
changed to `<value>`" or "checks `<state>` on head `<id>`" with the failing
names and URLs; a head whose checks are `none` or `pending` is not a
transition worth a message. When the change settled on this pass the
message ends by saying so, with the mirror's `Settled` reason. The message
carries `about`, the mirror's `uid`, `revision` and `head` it was computed
from, because by the
time it is delivered the change may have moved on; a worker reads a relay as
an observation at that revision, not as the present. `spec.forge.relay`
turns notes or checks off; a policy that depends on bot feedback relays bot
blockers regardless of `forge.relayBots` (mark-ready step).

Why the filters: this Task's own marked words are never relayed, because a
session sent its own reply answers itself. Another Fleet worker's marked
words are relayed, so two Tasks on one change can talk, and the label and
the brief tell a worker not to answer an acknowledgement that asks nothing.
A bot's notes are relayed only where configured or where the policy waits
on them, because coverage tables and preview links are status, not review.
A resolved thread is answered; a thread this Task already replied in is
answered. An unmarked note from Fleet's login is a person's until it says
otherwise.

## Mark ready

Design, not implemented: this section is the mark-ready step, which builds
on the `markReady` record and `spec.forge.draft.policy` the relay step
stores untouched.

Per entry, plan `MarkReady` when all of the following hold:

- a policy is selected: `spec.forge.draft.policy`, else `forge.draftPolicy`;
- `cr.state.value` is `draft` and `cr.everReady` is `false`: Fleet marks ready only
  a change that has been a draft the whole time it has been watched, so a
  change a person put back into draft, or a worker opened ready, is never
  touched;
- the mirror is fresh: `Reachable` is True and `observedAt` is within
  `2 × pollMs`;
- `entry.markReady.state` is `none`, or `failed` with fewer than 3 attempts;
- every predicate of the policy holds, `workerDone` and `workerIdle`
  included.

The plan writes `markReady = {state: pending, head: cr.head.id, attempts + 1}`
with the intent, under key `task/<name>/<uid>/ready/<change uid>/<head.id>`.
The effect carries the expected head and state, and the adapter re-reads
both immediately before the mutation and refuses if either moved; the
window between that read and the mutation is the forge's and is not closed
by Fleet.

| Outcome | Evidence | Next markReady state | Behavior |
| --- | --- | --- | --- |
| Known done | The receipt | `done` | Nothing more; the next ChangeRequest revision shows `open` and sets `everReady`. |
| Known failed, head or state moved | The receipt | `none` | Not an attempt against the change; re-evaluate on the next pass with the new head. |
| Known failed, other | The receipt | `failed` | Wait `RETRY_DELAY_MS` on this entry's `notBefore`; after 3 attempts `NeedsAttention/MarkReadyFailed`. |
| Unknown | The next fresh ChangeRequest revision | `done` if `everReady`, else `failed` | `everReady` is a recovered receipt. Still `draft` on a fresh read counts as a failed attempt; never repeated blindly. |

Once `everReady` is true, from Fleet's effect or anyone's hand, no follower
marks it ready again, which is what protects a person's return to draft
across every Task on the change.

## Retries and recovery

`MAX_ATTEMPTS=3` and `RETRY_DELAY_MS=30000` are the Task controller's
constants, reused; a wait is persisted on the entry's own `notBefore`, so a
failing mark-ready on one change holds back nothing else. `MarkReady` is the only
effect; its intent is written before it runs and its ambiguous outcome is
answered by the change's state, never by repeating it. A relay needs no
recovery: it is a status write.

## Conditions

Conditions live under `status.forge.conditions`, so the Task's own
`conditions` stay TaskController's. One `NeedsAttention` condition carries
every open reason, each naming its entry, in its message.

| Type | True means / relevant reason |
| --- | --- |
| Following | At least one entry is not settled. |
| Settled | Every entry has settled. |
| NeedsAttention | Reasons are `RelayGap`, `ChangeGone`, and, with the mark-ready step, `MarkReadyFailed` and `PolicyUnknown` (the selected policy is not in the config). The message holds one line per open reason, `<Reason> <entry> <text>`, and the reason is the one kind open or `Multiple`; an entry not observed on a pass keeps its lines. |

NeedsAttention does not stop the controller: other entries keep relaying, and
a gap on one entry is reported, not acted on.

## Deletion

Deleting a Task deletes nothing else. Its entries go with its record; the
ChangeRequests it followed stay for any other follower and are
garbage-collected by the
[ChangeRequest controller](../change-request-controller/state-machine.md#retention)
after their retention. ForgeController adds no finalizer.
