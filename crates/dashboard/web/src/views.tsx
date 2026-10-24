import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import * as Dialog from "@radix-ui/react-dialog";
import type { Action, Field, Resource, Row } from "./api";
import { columnsFor, type Column } from "./columns";
import { Donut, Gauge } from "./charts";
import {
  formatBytes,
  formatDate,
  formatNumber,
  isObjectArray,
  isRecord,
  label,
  pretty,
  shortId,
  text,
  toneFor,
} from "./format";

const PAGE_SIZE = 25;

export function Pill({ value }: { value: string }) {
  if (!value) return <span className="muted">—</span>;
  return <span className={`pill ${toneFor(value)}`}>{value}</span>;
}

export function Skeleton() {
  return (
    <div className="skeleton" aria-busy="true" aria-label="Loading">
      {[0, 1, 2, 3].map((row) => (
        <div className="skeleton-row" key={row} />
      ))}
    </div>
  );
}

export function Empty({ title, body }: { title: string; body: string }) {
  return (
    <div className="empty">
      <strong>{title}</strong>
      <p className="muted">{body}</p>
    </div>
  );
}

export type Toast = { id: number; tone: "ok" | "bad" | "info"; title: string; detail?: string };

export function Toasts({ items, onClose }: { items: Toast[]; onClose: (id: number) => void }) {
  if (items.length === 0) return null;
  return (
    <div className="toast-stack" aria-live="polite">
      {items.map((item) => (
        <div className={`toast ${item.tone}`} key={item.id}>
          <div className="toast-title">
            <strong>{item.title}</strong>
            <button type="button" onClick={() => onClose(item.id)} aria-label="Dismiss">
              Close
            </button>
          </div>
          {item.detail ? <pre className="code">{item.detail}</pre> : null}
        </div>
      ))}
    </div>
  );
}

function Scalar({ column, value }: { column: Column; value: unknown }) {
  if (value === null || value === undefined || value === "") return <span className="muted">—</span>;
  if (column.kind === "status") return <Pill value={text(value) || String(value)} />;
  if (typeof value === "string" && value.length < 24 && toneFor(value) !== "neutral") return <Pill value={value} />;
  if (column.kind === "bool" || typeof value === "boolean") {
    const on = Boolean(value);
    return <span className={on ? "yes" : "no"}>{on ? "Yes" : "No"}</span>;
  }
  if (column.kind === "bytes" && typeof value === "number") return <>{formatBytes(value)}</>;
  if (column.kind === "number" && typeof value === "number") return <>{formatNumber(value)}</>;
  if (column.kind === "date" && typeof value === "string") return <time dateTime={value} title={value}>{formatDate(value)}</time>;
  if (column.kind === "list" && Array.isArray(value)) {
    const joined = value.map((item) => text(item) || (item == null ? "" : "")).filter(Boolean).join(", ");
    return <span title={joined}>{joined || "—"}</span>;
  }
  const raw = text(value) || String(value);
  const shown = column.kind === "id" ? shortId(raw) : raw.length > 160 ? `${raw.slice(0, 157)}…` : raw;
  return <span title={raw}>{shown}</span>;
}

function linked(column: Column, value: unknown, children: ReactNode) {
  const query = text(value);
  if (!column.link || !query) return children;
  return (
    <Link to="/r/$id" params={{ id: column.link }} search={{ q: query }} onClick={(event) => event.stopPropagation()}>
      {children}
    </Link>
  );
}

export function ColumnChooser({ columns, shown, onToggle }: { columns: Column[]; shown: string[]; onToggle: (key: string) => void }) {
  return (
    <details className="chooser">
      <summary className="ghost">Columns</summary>
      <div>
        {columns.map((column) => (
          <label key={column.key}>
            <input type="checkbox" checked={shown.includes(column.key)} onChange={() => onToggle(column.key)} />
            {column.label}
          </label>
        ))}
      </div>
    </details>
  );
}

export function DataTable({
  rows,
  columns,
  shown,
  onRow,
  trailing,
  onPeek,
  resetKey = "",
}: {
  rows: Row[];
  columns: Column[];
  shown?: string[];
  onRow?: (row: Row) => void;
  trailing?: (row: Row) => ReactNode;
  onPeek?: (title: string, rows: Row[]) => void;
  resetKey?: string;
}) {
  const [page, setPage] = useState(0);
  const [sort, setSort] = useState<{ key: string; dir: 1 | -1 } | null>(null);
  useEffect(() => {
    setPage(0);
  }, [resetKey]);

  const visible = useMemo(() => {
    const keys = shown ?? columns.filter((column) => !column.hidden).map((column) => column.key);
    return columns.filter((column) => keys.includes(column.key));
  }, [columns, shown]);

  const ordered = useMemo(() => {
    if (!sort) return rows;
    const copy = [...rows];
    copy.sort((left, right) => {
      const av = left[sort.key];
      const bv = right[sort.key];
      if (av == null && bv == null) return 0;
      if (av == null) return 1;
      if (bv == null) return -1;
      if (typeof av === "number" && typeof bv === "number") return (av - bv) * sort.dir;
      if (typeof av === "boolean" && typeof bv === "boolean") return (Number(av) - Number(bv)) * sort.dir;
      return String(av).localeCompare(String(bv), "en", { numeric: true, sensitivity: "base" }) * sort.dir;
    });
    return copy;
  }, [rows, sort]);

  if (rows.length === 0) return <Empty title="Nothing here yet" body="Nothing matches this view." />;
  const pages = Math.max(1, Math.ceil(ordered.length / PAGE_SIZE));
  const safe = Math.min(page, pages - 1);
  const slice = ordered.slice(safe * PAGE_SIZE, safe * PAGE_SIZE + PAGE_SIZE);
  const start = safe * PAGE_SIZE + 1;
  const end = Math.min(ordered.length, start + PAGE_SIZE - 1);

  return (
    <>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              {visible.map((column) => (
                <th key={column.key} scope="col" aria-sort={sort?.key === column.key ? (sort.dir > 0 ? "ascending" : "descending") : "none"}>
                  {column.kind === "nested" ? (
                    column.label
                  ) : (
                    <button
                      type="button"
                      className="sort"
                      onClick={() =>
                        setSort((current) =>
                          current?.key === column.key ? { key: column.key, dir: current.dir > 0 ? -1 : 1 } : { key: column.key, dir: 1 },
                        )
                      }
                    >
                      {column.label}
                      {sort?.key === column.key ? (sort.dir > 0 ? " ↑" : " ↓") : ""}
                    </button>
                  )}
                </th>
              ))}
              {trailing ? <th scope="col">Actions</th> : null}
            </tr>
          </thead>
          <tbody>
            {slice.map((row, index) => (
              <tr key={text(row.id) || text(row.name) || String(safe * PAGE_SIZE + index)} className={onRow ? "click" : undefined} onClick={onRow ? () => onRow(row) : undefined}>
                {visible.map((column) => {
                  const value = row[column.key];
                  return (
                    <td key={column.key} data-label={column.label}>
                      {column.kind === "nested" && isObjectArray(value) ? (
                        <button
                          type="button"
                          className="chip"
                          onClick={(event) => {
                            event.stopPropagation();
                            onPeek?.(column.label, value);
                          }}
                        >
                          {value.length} {column.label.toLowerCase()}
                        </button>
                      ) : (
                        linked(column, value, <Scalar column={column} value={value} />)
                      )}
                    </td>
                  );
                })}
                {trailing ? (
                  <td className="actions" data-label="Actions">
                    {trailing(row)}
                  </td>
                ) : null}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {ordered.length > PAGE_SIZE ? (
        <div className="pager">
          <button type="button" disabled={safe === 0} onClick={() => setPage(safe - 1)}>
            Previous
          </button>
          <span className="muted">
            {start}–{end} of {ordered.length}
          </span>
          <button type="button" disabled={safe + 1 >= pages} onClick={() => setPage(safe + 1)}>
            Next
          </button>
        </div>
      ) : null}
    </>
  );
}

function Fact({ name, value }: { name: string; value: unknown }) {
  const column: Column = {
    key: name,
    label: label(name),
    kind:
      typeof value === "boolean"
        ? "bool"
        : typeof value === "number" && name.endsWith("_bytes")
          ? "bytes"
          : typeof value === "number"
            ? "number"
            : typeof value === "string" && looksStatus(name)
              ? "status"
              : typeof value === "string" && /^\d{4}-\d{2}-\d{2}/.test(value)
                ? "date"
                : "text",
  };
  return (
    <div>
      <dt>{label(name)}</dt>
      <dd>
        <Scalar column={column} value={value} />
      </dd>
    </div>
  );
}

function looksStatus(key: string): boolean {
  return key === "state" || key === "status" || key === "phase" || key.endsWith("_state") || key.endsWith("_status") || key.endsWith("_phase");
}

export function RecordView({ value, depth = 0 }: { value: unknown; depth?: number }) {
  if (Array.isArray(value)) {
    if (isObjectArray(value)) return <NestedTable rows={value} />;
    return <p>{value.map((item) => text(item)).filter(Boolean).join(", ") || "—"}</p>;
  }
  if (!isRecord(value)) return <p>{text(value) || "—"}</p>;
  const entries = Object.entries(value).filter(([, item]) => item !== null && item !== undefined && item !== "");
  const scalars = entries.filter(([, item]) => !isRecord(item) && !Array.isArray(item));
  const lists = entries.filter(([, item]) => Array.isArray(item));
  const objects = entries.filter(([, item]) => isRecord(item));
  const long = scalars.filter(([, item]) => typeof item === "string" && item.length > 180);
  const short = scalars.filter((entry) => !long.includes(entry));
  const percent = typeof value.used_percent === "number" ? value.used_percent : null;
  return (
    <div className="stack">
      {percent !== null ? (
        <Gauge
          id={`used-${depth}-${String(value.name ?? "cap").replace(/[^A-Za-z0-9_-]/g, "")}`}
          percent={percent}
          caption={text(value.used_human) ? `${text(value.used_human)} used` : undefined}
        />
      ) : null}
      {short.length ? (
        <dl className="facts">
          {short.map(([key, item]) => (
            <Fact key={key} name={key} value={item} />
          ))}
        </dl>
      ) : null}
      {long.map(([key, item]) => (
        <section key={key}>
          <h3>{label(key)}</h3>
          <pre className="code">{String(item)}</pre>
        </section>
      ))}
      {lists.map(([key, item]) => (
        <section key={key} className="stack">
          <h3>{label(key)}</h3>
          {isObjectArray(item) ? <NestedTable rows={item} /> : <p>{(item as unknown[]).map((entry) => text(entry)).filter(Boolean).join(", ") || "—"}</p>}
        </section>
      ))}
      {depth < 2
        ? objects.map(([key, item]) => (
            <section key={key} className="stack">
              <h3>{label(key)}</h3>
              <RecordView value={item} depth={depth + 1} />
            </section>
          ))
        : null}
    </div>
  );
}

function NestedTable({ rows }: { rows: Row[] }) {
  const columns = useMemo(() => columnsFor("", rows), [rows]);
  const [peek, setPeek] = useState<{ title: string; rows: Row[] } | null>(null);
  return (
    <>
      <DataTable rows={rows} columns={columns} onPeek={(title, nested) => setPeek({ title, rows: nested })} />
      <PeekDialog peek={peek} onClose={() => setPeek(null)} />
    </>
  );
}

export function PeekDialog({ peek, onClose }: { peek: { title: string; rows: Row[] } | null; onClose: () => void }) {
  const columns = useMemo(() => (peek ? columnsFor("", peek.rows) : []), [peek]);
  return (
    <Dialog.Root open={peek !== null} onOpenChange={(open) => { if (!open) onClose(); }}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className="dialog wide">
          <Dialog.Title>{peek?.title ?? "Items"}</Dialog.Title>
          {peek ? <DataTable rows={peek.rows} columns={columns} /> : null}
          <div className="buttons">
            <Dialog.Close asChild>
              <button type="button">Close</button>
            </Dialog.Close>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

export function DetailDrawer({
  open,
  title,
  row,
  body,
  loading,
  error,
  actions,
  onClose,
}: {
  open: boolean;
  title: string;
  row: Row | null;
  body: unknown;
  loading: boolean;
  error: string;
  actions?: ReactNode;
  onClose: () => void;
}) {
  const [raw, setRaw] = useState(false);
  useEffect(() => {
    setRaw(false);
  }, [row]);
  const detail = isRecord(body) && ("detail" in body || "spec" in body) ? body.detail : body;
  const spec = isRecord(body) ? body.spec : undefined;
  return (
    <Dialog.Root open={open} onOpenChange={(next) => { if (!next) onClose(); }}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className="dialog drawer">
          <header className="drawer-head">
            <Dialog.Title>{title}</Dialog.Title>
            <div className="tabs">
              <button type="button" className={raw ? "" : "primary"} onClick={() => setRaw(false)}>
                Details
              </button>
              <button type="button" className={raw ? "primary" : ""} onClick={() => setRaw(true)}>
                JSON
              </button>
            </div>
          </header>
          {actions}
          {loading ? <p className="muted">Loading the full record…</p> : null}
          {error ? <p className="error">{error}</p> : null}
          <div className="drawer-body">
            {raw ? (
              <pre className="code">{pretty(body ?? row)}</pre>
            ) : (
              <div className="stack">
                <RecordView value={detail ?? row ?? {}} />
                {isRecord(spec) ? (
                  <section className="stack">
                    <h3>Spec</h3>
                    <RecordView value={spec} />
                  </section>
                ) : null}
              </div>
            )}
          </div>
          <div className="buttons">
            <Dialog.Close asChild>
              <button type="button">Close</button>
            </Dialog.Close>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function splitActions(actions: Action[]) {
  const primary = actions.find((action) => !action.confirm && !action.client && action.fields.length === 0 && action.name !== "delete");
  return { primary, rest: actions.filter((action) => action !== primary) };
}

export function RowActions({
  resource,
  row,
  onOpen,
  onRun,
  onInspect,
}: {
  resource: Resource;
  row?: Row;
  onOpen: (
    title: string,
    fields: Field[],
    submit: (form: Record<string, string | boolean>) => Promise<void>,
    options?: { checks?: Action[]; preload?: boolean; confirm?: boolean; requireForce?: boolean; row?: Row },
  ) => void;
  onRun: (action: Action, row: Row, form?: Record<string, string | boolean>) => Promise<void>;
  onInspect: (row: Row) => void;
}) {
  const [menu, setMenu] = useState<{ top: number; left: number } | null>(null);
  const { primary, rest } = splitActions(resource.actions);
  const idValue = row ? text(row[resource.idField]) : "";
  useEffect(() => {
    if (!menu) return;
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") setMenu(null);
    }
    function onPointer(event: MouseEvent) {
      const target = event.target as HTMLElement | null;
      if (!target?.closest(".menu") && !target?.closest(".menu-button")) setMenu(null);
    }
    window.addEventListener("keydown", onKey);
    window.addEventListener("mousedown", onPointer);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("mousedown", onPointer);
    };
  }, [menu]);

  function activate(action: Action) {
    setMenu(null);
    if (!row) return;
    if (action.client) return;
    if (action.fields.length || action.confirm || action.preload) {
      onOpen(action.label, action.fields, (form) => onRun(action, row, form), {
        confirm: action.confirm,
        preload: action.preload,
        requireForce: action.name === "reset-migrate",
        row,
      });
      return;
    }
    onRun(action, row).catch(() => undefined);
  }

  const items = (
    <>
      {row && resource.inspect ? (
        <button type="button" role="menuitem" onClick={() => { setMenu(null); onInspect(row); }}>
          Open
        </button>
      ) : null}
      {rest.map((action) =>
        action.client ? (
          <a key={action.name} role="menuitem" href={`/vms/${idValue}/console`}>
            {action.label}
          </a>
        ) : (
          <button type="button" role="menuitem" key={action.name} className={action.confirm ? "danger" : ""} onClick={() => activate(action)}>
            {action.label}
          </button>
        ),
      )}
    </>
  );
  const hasMenu = Boolean(row && resource.inspect) || rest.length > 0;

  return (
    <div className="row-actions" onClick={(event) => event.stopPropagation()}>
      {primary && row ? (
        <button type="button" onClick={() => activate(primary)}>
          {primary.label}
        </button>
      ) : null}
      {hasMenu ? (
        <button
          type="button"
          className="menu-button"
          aria-haspopup="menu"
          aria-expanded={menu !== null}
          onClick={(event) => {
            const rect = event.currentTarget.getBoundingClientRect();
            const top = rect.bottom + 220 > window.innerHeight ? Math.max(8, rect.top - 200) : rect.bottom + 4;
            setMenu({ top, left: Math.max(8, rect.right - 220) });
          }}
        >
          Actions
        </button>
      ) : null}
      {menu ? (
        <div className="menu" role="menu" style={{ top: menu.top, left: menu.left }}>
          {items}
        </div>
      ) : null}
    </div>
  );
}

function Metric({ name, value }: { name: string; value: unknown }) {
  const shown = typeof value === "number" ? formatNumber(value) : typeof value === "boolean" ? (value ? "Yes" : "No") : text(value) || "—";
  const compact = typeof value === "number" || typeof value === "boolean" || shown.length <= 18;
  return (
    <div className="card">
      <span className="muted">{label(name)}</span>
      <strong className={compact ? "metric" : ""}>{shown}</strong>
    </div>
  );
}

function strings(value: unknown): string[] {
  return Array.isArray(value) ? value.map((item) => text(item) || (typeof item === "number" ? String(item) : "")).filter(Boolean) : [];
}

export function SummaryPage({ id, row, onPeek }: { id: string; row: Row; onPeek: (title: string, rows: Row[]) => void }) {
  if (id === "health" || id === "cluster") return <HealthBoard row={row} />;
  if (id === "storage") return <StorageBoard row={row} onPeek={onPeek} />;
  if (id === "network") return <NetworkBoard row={row} onPeek={onPeek} />;
  if (id === "compliance") return <ComplianceBoard row={row} onPeek={onPeek} />;
  if (id === "replication") return <ReplicationBoard row={row} />;
  return <RecordView value={row} />;
}

function HealthBoard({ row }: { row: Row }) {
  const status = text(row.status) || "unknown";
  const conditions = strings(row.conditions);
  return (
    <div className="stack">
      <div className={`banner ${toneFor(status)}`}>
        <Pill value={status} />
        <strong>{text(row.headline) || "Cluster"}</strong>
      </div>
      <div className="grid">
        {["ready_nodes", "not_ready_nodes", "pending_nodes", "unschedulable_nodes", "vm_count", "vms_on_not_ready_nodes", "replication_conflicts", "certificates_expiring_soon"].map((key) =>
          row[key] === undefined ? null : <Metric key={key} name={key} value={row[key]} />,
        )}
      </div>
      <div className="split">
        <section className="card stack">
          <h2>High availability</h2>
          <dl className="facts">
            {["host_failures_tolerated", "failover_capacity", "ha_lease_holder", "unprotected_on_lost_nodes"].map((key) =>
              row[key] === undefined ? null : <Fact key={key} name={key} value={row[key]} />,
            )}
          </dl>
        </section>
        <section className="card stack">
          <h2>Conditions</h2>
          {conditions.length ? (
            <ul className="alerts">
              {conditions.map((condition) => (
                <li key={condition}>{condition}</li>
              ))}
            </ul>
          ) : (
            <p className="muted">No conditions reported.</p>
          )}
        </section>
      </div>
    </div>
  );
}

function StorageBoard({ row, onPeek }: { row: Row; onPeek: (title: string, rows: Row[]) => void }) {
  const nodes = isObjectArray(row.nodes) ? row.nodes : [];
  const columns = useMemo(() => columnsFor("nodes", nodes), [nodes]);
  const slices = [
    ["Filesystem", row.backend_filesystem_nodes, "#6366f1"],
    ["LVM", row.backend_lvm_nodes, "#8b5cf6"],
    ["ZFS", row.backend_zfs_nodes, "#3b82f6"],
    ["Ceph", row.backend_ceph_nodes, "#10b981"],
    ["Unspecified", row.backend_unspecified_nodes, "#71717a"],
  ].filter((entry) => typeof entry[1] === "number" && entry[1] > 0) as [string, number, string][];
  return (
    <div className="stack">
      <div className="grid">
        {["approved_nodes", "nodes_disk_inventory_ok", "total_block_devices", "nodes_luks_keyfile", "nodes_luks_unknown"].map((key) =>
          row[key] === undefined ? null : <Metric key={key} name={key} value={row[key]} />,
        )}
      </div>
      {slices.length ? (
        <section className="card">
          <h2>Backends</h2>
          <Donut slices={slices.map(([name, value, color]) => ({ label: name, value, color }))} />
        </section>
      ) : null}
      {nodes.length ? (
        <section className="stack">
          <h2>Nodes</h2>
          <DataTable rows={nodes} columns={columns} onPeek={onPeek} />
        </section>
      ) : null}
    </div>
  );
}

function NetworkBoard({ row, onPeek }: { row: Row; onPeek: (title: string, rows: Row[]) => void }) {
  const nodes = isObjectArray(row.nodes) ? row.nodes : [];
  const columns = useMemo(() => columnsFor("", nodes), [nodes]);
  return (
    <div className="stack">
      <div className="grid">
        {["default_gateway_interface", "default_external_ip", "default_gateway_ip", "default_internal_netmask"].map((key) =>
          row[key] === undefined || row[key] === "" ? null : <Metric key={key} name={key} value={row[key]} />,
        )}
      </div>
      {nodes.length ? (
        <section className="stack">
          <h2>Host interfaces</h2>
          <DataTable rows={nodes} columns={columns} onPeek={onPeek} />
        </section>
      ) : (
        <RecordView value={row} />
      )}
    </div>
  );
}

const COMPLIANCE_GROUPS: { title: string; keys: string[] }[] = [
  { title: "Controller", keys: ["controller_version", "crypto_library", "mtls_enabled", "sub_ca_enabled", "cert_auto_renewal_days"] },
  { title: "Nodes", keys: ["total_nodes", "approved_nodes", "pending_nodes", "rejected_nodes", "nodes_cert_unknown", "nodes_luks_keyfile", "nodes_luks_unknown"] },
  { title: "Workloads", keys: ["total_vms", "running_vms", "stopped_vms", "total_networks", "nat_networks", "bridge_networks", "vxlan_networks"] },
];

function ComplianceBoard({ row, onPeek }: { row: Row; onPeek: (title: string, rows: Row[]) => void }) {
  const tables = Object.entries(row).filter((entry): entry is [string, Row[]] => isObjectArray(entry[1]));
  return (
    <div className="stack">
      {COMPLIANCE_GROUPS.map((group) => (
        <section key={group.title} className="stack">
          <h2>{group.title}</h2>
          <div className="grid">
            {group.keys.filter((key) => row[key] !== undefined).map((key) => (
              <Metric key={key} name={key} value={row[key]} />
            ))}
          </div>
        </section>
      ))}
      {tables.map(([key, nested]) => (
        <section key={key} className="stack">
          <h2>{label(key)}</h2>
          <DataTable rows={nested} columns={columnsFor("", nested)} onPeek={onPeek} />
        </section>
      ))}
    </div>
  );
}

function ReplicationBoard({ row }: { row: Row }) {
  const violations = strings(row.zero_manual_slo_violations);
  const outgoing = isObjectArray(row.outgoing) ? row.outgoing : [];
  const incoming = isObjectArray(row.incoming) ? row.incoming : [];
  const skip = new Set(["outgoing", "incoming", "zero_manual_slo_violations"]);
  const rest = Object.fromEntries(Object.entries(row).filter(([key]) => !skip.has(key)));
  return (
    <div className="stack">
      <RecordView value={rest} />
      {violations.length ? (
        <section className="card stack">
          <h2>SLO violations</h2>
          <ul className="alerts">
            {violations.map((item) => (
              <li key={item}>{item}</li>
            ))}
          </ul>
        </section>
      ) : null}
      {outgoing.length ? (
        <section className="stack">
          <h2>Outgoing</h2>
          <DataTable rows={outgoing} columns={columnsFor("", outgoing)} />
        </section>
      ) : null}
      {incoming.length ? (
        <section className="stack">
          <h2>Incoming</h2>
          <DataTable rows={incoming} columns={columnsFor("", incoming)} />
        </section>
      ) : null}
    </div>
  );
}
