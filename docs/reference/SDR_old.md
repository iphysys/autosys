# Structured Design Representation

## Purpose and implementation status

The Structured Design Representation (SDR) is the authoritative, session-local
design state. It records design intent, stable identities, relationships,
properties, constraints, external artifacts, evidence, and engineering decisions.
CAD is a derived artifact, not the source of design truth.

The current implementation provides schema-driven records, six single-operation
mutations, immutable revision snapshots, dependency invalidation, a JSON tool
facade, and a direct Build123D HTTP adapter. It is not a complete SysML implementation
or a general engineering certification system.

**Atomic multi-operation transactions, SysML-inspired semantic completeness, and
a design-level verification gate are planned, not implemented.** This guide
distinguishes the available API from those completion requirements.

## Architecture and ownership

| Layer | Responsibility | Entry point |
| --- | --- | --- |
| Metamodel | Authoritative record fields, primitive constraints, and vocabulary | [schema/](./schema/) |
| Runtime models | Cached Pydantic classes and trusted runtime policies | [models.py](./models.py), [schema_loader.py](./schema_loader.py) |
| Operations | Typed requests with an engineering reason | [operations.py](./operations.py) |
| Store | Audited mutation, publication, snapshots, and revision comparison | [store.py](./store.py) |
| Validation | Record integrity, dependency ordering, paths, and restricted constraint checks | [validation.py](./validation.py) |
| Tool facade | Session-scoped JSON requests and structured responses | [design_tools.py](./design_tools.py) |
| CAD boundary | Trusted script execution and checked validation-report registration | [cad_adapter.py](./cad_adapter.py) |
| Behavior examples | Plate/car recipes and deterministic lifecycle analysis | [examples/](./examples/) |

Production modules do not import the examples or the Build123D Python package.
The adapter calls the existing private CAD service; geometry recipes and
engineering acceptance criteria belong to the caller's validator script.
Deployments must explicitly exclude examples and tests if they distribute the
whole SDR directory. Their location alone is not a packaging exclusion.

## Runtime and storage contract

- Use the repository's locked environment. Python 3.12 is the reference runtime;
  the project advertises Python 3.10 through 3.13, but that range is not certified
  by a single local test run. Some CAD examples use Python 3.11+ helpers.
- Pydantic 2 is required. The host-side SDR does not require a CAD kernel.
- Construct a store at a trusted `session_root / "sdr"`. Constructing or loading
  a store does not allocate a session.
- Each design has a `current.json` and immutable
  `revisions/revision_NNNNNN.json` snapshots beneath its design ID.
- Creation produces empty revision zero. Each successful current mutation adds
  one revision and one decision. Identical-value property edits still record
  intent and invalidate consumers.
- Direct store access is **single-writer**. The tool facade uses a per-session
  cooperative lock around design creation and revision-check/mutation.
  Stale-state checks alone are not concurrent-writer coordination.
  Use canonical lowercase session IDs on case-insensitive storage.
- Publication stages and fsyncs files, publishes the snapshot exclusively, then
  replaces current state atomically. This is not a guarantee against power loss,
  hostile filesystem races, or external tampering.

## Record model

| Record | Required caller fields | Role |
| --- | --- | --- |
| Design | `name`, `objective` | Root state with schema version, revision, record collections, and decisions |
| Entity | `entity_type`, `class_name`, `name` | Mission, requirement, function, system, subsystem, component, interface, analysis, or test |
| PropertyValue | `value` | Non-null JSON value, optional unit/type label, and store-managed provenance |
| Relationship | `relationship_type`, `source_id`, `target_id` | Directed, non-self-referential link |
| Constraint | `name`, `kind`, `expression` | Declared property check with optional requirement linkage |
| Artifact | `artifact_type`, `format`, `uri` | External payload reference, dependencies, checksum, and producer metadata |
| Evidence | `claim`, `subject_id`, `method` | Result and observation linked to supporting artifacts |
| Decision | Store-managed during mutation | Reason, actor, old/new values, provenance references, and revision |

IDs are stable identities, not display names. Collection keys must match record
IDs, and identities must be unique across the design. Record references use IDs;
property references use `entity_id.property_name`.

Controlled lifecycle states are `proposed`, `active`, `stale`, `valid`, `invalid`,
and `retired`. Status labels alone do not establish verification.

Units and `value_type` are descriptive labels in the generic metamodel.
They do not implement automatic conversion or class-specific dimensional rules.
The authoritative definitions and exact defaults are in
[design.json](./schema/design.json) and [records.json](./schema/records.json).

## Mutation API

`DesignStore.apply(design, operation)` accepts a typed operation or decoded
operation dictionary and returns a new persisted Design. The supplied design
must match current state. Do not mutate a loaded object and use `save` as a patch.

Every operation requires `operation` and a visible, nonblank `reason`.
`actor` defaults to `agent`; optional `addresses_ids` and `based_on_ids` reference
existing records. Actor values are provenance claims, not authentication.

| Operation | Required payload | Behavior |
| --- | --- | --- |
| `create_entity` | `entity` | Insert a new entity with a unique ID |
| `update_property` | `entity_id`, `property_name`, `value` | Add or replace one entity property and invalidate declared consumers |
| `create_relationship` | `relationship` | Insert a directed link whose endpoints exist |
| `add_constraint` | `constraint` | Insert and evaluate a declared constraint |
| `attach_artifact` | `artifact` | Register external metadata; does not read or certify the payload |
| `record_evidence` | `evidence` | Register an observation/claim; does not authenticate verification |

For property updates, omit `value.source_decision_id`; the store assigns it.
The store also assigns mutation timestamps. New artifacts and evidence should
declare their actual input dependencies explicitly.

There is currently no batch, arbitrary document replacement, record deletion,
metadata-edit, or general lifecycle-transition operation.

### Store methods

| Method | Result and contract |
| --- | --- |
| `DesignStore(root)` | Pin a trusted SDR root; does not create directories |
| `create(name, objective)` | Persist an empty revision-zero Design |
| `load(design_id)` | Return a freshly validated Design; missing/corrupt state raises an error |
| `apply(design, operation)` | Persist one audited operation and return its new state |
| `validate(design)` | Return integrity/path errors; does not read payloads or certify engineering outcomes |
| `save(design)` | Create a new empty design or accept unchanged existing state; reject replacement |
| `snapshot(design)` | Archive matching current state or return its identical existing snapshot |
| `diff(design_id, from_revision, to_revision)` | Compare committed snapshots; unavailable or future revisions fail |

Diff results contain `design_id`, `from_revision`, `to_revision`, `added`,
`removed`, and `changed`. Added/removed entries are sorted IDs by collection.
Changed entries use dotted field paths with `old` and `new` JSON values.

## JSON tool API

`DesignTools(session_root).call(name, arguments)` is the validated JSON boundary.
Use it instead of invoking facade methods with incomplete argument sets.
`TOOL_SCHEMAS` contains the generated function schemas.

| Tool | Arguments | Output |
| --- | --- | --- |
| `design_get` | None to list; `design_id` to inspect; or both `name` and `objective` to create | Design list, metadata/record IDs, or creation metadata |
| `design_query` | `design_id`; optional entity IDs/types/classes and inclusion flags | Selected entities, internal relationships, touching constraints, artifacts, and evidence |
| `design_apply` | `design_id`, nonnegative `expected_revision`, one `operation` | New metadata and the committed decision |
| `design_validate` | `design_id` | Metadata, integrity `valid`, `errors`, and separate `constraint_checks` |
| `design_diff` | `design_id`, `from_revision`, `to_revision` | Revision comparison |

Query filters combine with AND. `include_neighbors` defaults to false and adds
one relationship hop; `include_artifacts` and `include_evidence` default to true.
Unknown requested entity IDs are errors, not empty success results.

Successful calls return `ok: true`, `data`, and an empty `errors` list.
Rejected calls return `ok: false`, `data: null`, structured errors, and a message.
Field-level errors preserve `loc`, `type`, and `msg`.

The facade forces mutation actor to `agent`. The interactive `agent_loop.py`
dispatches the five tools directly to `DesignTools(session_root)`; the separate
HTTP agent runtime and frontend do not expose them automatically.

Design creation and `design_apply` use POSIX `flock` on
`session_root/sdr/.mutation.lock`, including the current-revision check and
publication. Use Linux, macOS, WSL, or the Linux agent image. The lock coordinates
threads and processes using this facade; direct `DesignStore` calls and file edits
do not participate. Reads and facade construction do not create a lock file.

There is no dedicated writer process, account, socket, API key, or protected SDR
volume. SDR and referenced artifacts live in the shared session workspace.
The lock is **not access control**: Bash/CAD tools or other processes with write
access can bypass it. A request key cannot prevent those filesystem writes.
API snapshots are immutable through supported store operations, not OS permissions.

Artifact attachment references the original session-relative file; it does not
archive bytes or authenticate numeric readbacks. Evidence status is a caller claim.
`design_validate` reports integrity and constraint outcomes, not a verification
gate or the former writer's `checks_passed` result.

Old authority volumes are not deleted or automatically migrated. Their existing
designs remain outside the restored session-local store. Rebuild the agent image
after switching from the writer-based entrypoint.

`format_design_result` may return an `output_limit` error after a mutation has
already committed. Inspect `committed_revision`, reload, and narrow the query;
never treat response formatting failure as proof that no mutation occurred.

## Validation and freshness

| Check | Established | Not established |
| --- | --- | --- |
| Generated models | Primitive types, field constraints, controlled fields | Engineering correctness |
| Integrity validation | IDs, references, provenance, dependency ordering, path containment | File existence, checksum agreement, trusted actor identity |
| Constraint evaluation | Restricted expression outcome on declared properties | General simulation or unrestricted unit algebra |
| CAD adapter | Successful service execution and a report matching pinned input | Validator honesty, arbitrary CAD policy, design-wide certification |

Constraint outcomes are `passed`, `failed`, `error`, or `skipped`. Expressions
support bounded arithmetic, comparisons, membership, and literal lists/tuples.
Calls, arbitrary Python execution, automatic unit conversion, and general
engineering equations are unsupported.

Adding a constraint or editing its inputs updates applicable stored outcomes.
Failed constraints can currently be persisted as `invalid`. An integrity-valid
design can therefore contain failed engineering checks. Always inspect outcomes;
`design_validate.valid` is not a verification gate. Invalid saved documents may
fail during load instead of returning a normal validation response.

Artifacts consume `depends_on_ids` and `generated_from_ids`; evidence consumes
`depends_on_ids` and `artifact_ids`; constraints consume `referenced_paths`.
Active/valid `depends_on` relationships make their source consume their target.
Containment, subject labels, and decision provenance do not implicitly create
freshness dependencies.

Changed inputs invalidate dependents transitively. Old observations and external
files remain historical records. Regenerate into new paths, register new outputs,
and rerun verification; never revive old evidence merely by changing its status.

## Build123D adapter API

The public adapter types are exported from `sdr`. Configure the service URL in
trusted application bootstrap code, normally from `BUILD123D_TOOL_URL`.
The adapter does not discover configuration or accept an agent-selected service.

| API | Contract |
| --- | --- |
| `CadAdapter(store, workspace_root, service_url, timeout_s=330)` | Configure a session store, the host-visible shared workspace, and an HTTP(S) origin |
| `execute(script_uri)` | POST a trusted Python script to `/execute`; return `CadExecutionResult` or raise `CadAdapterError` |
| `validate_and_record(design, artifact_id, validation_script_uri=..., report_uri=..., reason=...)` | Execute validation, check pinned inputs/report, register a report artifact and evidence, and return `CadValidationResult` |

Script, artifact, and report URIs are relative to the **session root**.
The adapter translates the script to a shared-workspace-relative service path.
The service mounts the same workspace at `/workspace` and executes with the
script's parent as its working directory.

The artifact must already be registered as `cad_model`, have a matching SHA-256
checksum, and have no stale, invalid, or retired transitive inputs.
The validator must be a trusted Python file. The report must use a new path
distinct from the artifact and validator. The caller prepares its parent directory.

### Validation report contract

The validator writes a JSON object to `report_uri`. The report schema is available
through `CadValidationReport.model_json_schema()`.

| Field | Requirement |
| --- | --- |
| `schema_version` | Exactly `cad-validation/0.1` |
| `input_revision` | Current pinned nonnegative SDR revision |
| `artifact_id` | ID of the CAD artifact being validated |
| `artifact_checksum` | `sha256:` followed by 64 lowercase hexadecimal characters; must match actual bytes |
| `claim` | Nonblank statement of the checks performed |
| `method` | Nonblank validator method identifier |
| `tool_version` | Nonblank Build123D/tool version |
| `result` | Strict boolean engineering outcome |
| `observed` | Optional PropertyValue; omit decision provenance |
| `required` | Optional JSON object describing acceptance criteria; defaults to empty |

Unknown fields, duplicate JSON keys, malformed values, reports over 65,536 bytes,
changed input files/scripts, stale revisions, and mismatched identities/checksums
are rejected before report/evidence registration.

HTTP success is not engineering success. A successful script with `result: false`
produces evidence with `status: invalid` and a receipt with `passed: false`.
Execution failure or a rejected report produces no new evidence.
The receipt contains `design`, `report_artifact_id`, `evidence_id`, and `passed`.

The report artifact records its checksum, input revision, validator URI, and
validator checksum. Evidence links the original CAD artifact and the report.
Freshness then propagates through the existing SDR dependency mechanism.
The adapter does not change the original CAD artifact's lifecycle status.

**Registration currently uses two independent audited mutations.** A late
evidence-publication failure can leave the report committed. Reload current state
and inspect decisions before recovery. Use a fresh output path for another run;
do not blindly retry or delete a referenced report. Atomic registration requires
the planned transaction API.

## Operations required for completion

The following are required capabilities, not callable APIs in this release.
Their final request names and precise policy must be defined before implementation.

| Capability | Required behavior | Completion criterion |
| --- | --- | --- |
| Atomic multi-operation transaction | Apply related operations to a candidate, validate final state, then publish once | One revision/snapshot per transaction, per-operation decisions linked to it, whole-transaction rejection |
| Entity/property editing | Audited metadata updates and controlled property removal | Dependency invalidation and provenance remain correct |
| Relationship maintenance | Audited edits and retirement of obsolete links | Endpoint semantics, dependencies, and history remain consistent |
| Constraint maintenance | Audited definition changes, retirement, and explicit reevaluation | Distinguish violation, evaluator error, and unavailable inputs |
| Lifecycle transitions | Controlled retirement, supersession, and reactivation | No unvalidated transition can manufacture current validity |
| SysML-inspired semantic validation | Applicable decomposition/interface rules, function allocation, requirement satisfaction and verification links | Every committed transaction satisfies the documented project policy |
| Verification gate | Evaluate applicable checks and trusted, fresh supporting evidence | Stale/missing/failed evidence cannot establish a verified claim |
| Atomic CAD-result registration | Commit related report/evidence records together | No report-only partial registration |

The agreed scope is generic SDR JSON with SysML-inspired project rules, not
formal SysML language conformance. Explicitly stale outputs remain permissible
after input edits, but cannot support a verified claim.
Prefer retirement/supersession over destructive deletion so traceability survives.

## Examples and validation commands

Run commands from the repository root after restoring the locked development
environment with `uv sync --group dev`.

| Purpose | Command |
| --- | --- |
| SDR regression and adapter unit tests | `uv run pytest src/sdr/tests -q` |
| Schema acceptance/rejection example | `uv run python src/sdr/examples/schema_demo.py workspace/sessions/schema-example` |
| Constraint and persistence behavior | `uv run python src/sdr/examples/demo1.py` through `demo7.py`, individually |
| Persistent real Plate lifecycle | `uv run python src/sdr/examples/cad_lifecycle.py` |
| Concept-car behavior example | `uv run python src/sdr/examples/car_lifecycle.py` |
| Live Plate integration | `RUN_CAD_INTEGRATION=1 uv run pytest src/sdr/tests/test_cad.py::test_plate_to_step_to_evidence_to_invalidation -q` |
| Live HTTP adapter integration | `RUN_CAD_INTEGRATION=1 CAD_TEST_SERVICE_URL=http://reachable-cad-host:8000 uv run pytest src/sdr/tests/test_cad_adapter.py::test_live_http_adapter_records_real_step_evidence -q` |

The CAD container must be healthy and share the selected workspace. Plate
integration defaults to container `autonomous_design-build123d-1`; override
`CAD_TEST_CONTAINER` or `CAD_TEST_WORKSPACE` when required.
The HTTP adapter test additionally requires a service origin reachable from its
test process. Never expose the private execution service publicly for testing.

Unit tests use fixture CAD/report data and do not prove geometry validity.
Live tests execute actual Build123D export/reimport. Examples preserve their own
outputs; integration tests use isolated temporary sessions.

## Failure handling and operational boundaries

- Handle `DesignStoreError` for persistence/integrity rejection and
  `CadAdapterError` for adapter/configuration/service/report failure.
- Reload after a stale revision, partial CAD registration, or ambiguous response;
  do not blindly repeat a mutation.
- Malformed or failed CAD runs can leave external output files, ev