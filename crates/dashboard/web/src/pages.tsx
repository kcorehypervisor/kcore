import { useDeferredValue, useEffect, useMemo, useRef, useState } from "react";
import { Link, useParams, useSearch } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import * as Dialog from "@radix-ui/react-dialog";
import { api, payload, specToValues, type Action, type Catalog, type Field, type Resource, type Row } from "./api";
import { columnsFor } from "./columns";
import { Bars, Donut, Meter } from "./charts";
import { displayTitle, formatDate, pretty, rowTitle, text, toneColor, toneFor, usedPercent } from "./format";
import {
  ColumnChooser,
  DataTable,
  DetailDrawer,
  Empty,
  PeekDialog,
  RowActions,
  Skeleton,
  SummaryPage,
  Toasts,
  type Toast,
} from "./views";

const ESSENTIAL = new Set([
  "name",
  "cpu",
  "memory_bytes",
  "storage_backend",
  "storage_size_bytes",
  "image_url",
  "image_path",
  "desired_state",
]);

const LIST_COLUMNS: Record<string, { name: string; kind?: "bool" | "number" }[]> = {
  nics: [{ name: "network" }, { name: "model" }, { name: "mac_address" }],
  disks: [{ name: "name" }, { name: "backend_handle" }, { name: "bus" }, { name: "device" }],
  rules: [
    { name: "protocol" },
    { name: "host_port", kind: "number" },
    { name: "target_port", kind: "number" },
    { name: "source_cidr" },
    { name: "target_vm" },
    { name: "enable_dnat", kind: "bool" },
  ],
  control_plane_nodes: [
    { name: "node_id" },
    { name: "advertise_address" },
    { name: "iface" },
    { name: "initial_cluster", kind: "bool" },
    { name: "join_existing", kind: "bool" },
  ],
  worker_nodes: [{ name: "node_id" }, { name: "advertise_address" }, { name: "iface" }],
  nodes: [
    { name: "node_id" },
    { name: "osd_device" },
    { name: "osd_devices" },
    { name: "mon_addr" },
    { name: "public_iface" },
    { name: "cluster_iface" },
  ],
};

function parseList(raw: string | boolean | undefined): Record<string, string | boolean>[] {
  if (typeof raw !== "string" || !raw.trim()) return [];
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((item) => item && typeof item === "object") as Record<string, string | boolean>[];
  } catch {
    return [];
  }
}

function haystack(row: Row): string {
  const parts: string[] = [];
  let size = 0;
  const visit = (value: unknown, depth: number) => {
    if (size > 4000 || value == null || depth > 2) return;
    if (typeof value !== "object") {
      parts.push(String(value));
      size += String(value).length;
      return;
    }
    const items = Array.isArray(value) ? value.slice(0, 40) : Object.values(value);
    for (const item of items) visit(item, depth + 1);
  };
  visit(row, 0);
  return parts.join("\u0000").toLowerCase();
}

function detailOf(value: unknown): string | undefined {
  if (value && typeof value === "object" && !Array.isArray(value)) {
    const record = value as Row;
    if (Object.keys(record).length === 1 && record.ok === true) return undefined;
  }
  return pretty(value);
}

const STATE_COLORS = ["#10b981", "#f59e0b", "#f87171", "#6366f1", "#8b5cf6", "#3b82f6"];

export function Overview() {
  const health = useQuery({
    queryKey: ["rows", "health", ""],
    queryFn: () => api<{ rows: Row[] }>("/api/v1/resources/health"),
  });
  const compliance = useQuery({
    queryKey: ["rows", "compliance", ""],
    queryFn: () => api<{ rows: Row[] }>("/api/v1/resources/compliance"),
  });
  const vms = useQuery({
    queryKey: ["rows", "vms", ""],
    queryFn: () => api<{ rows: Row[] }>("/api/v1/resources/vms"),
  });
  const nodes = useQuery({
    queryKey: ["rows", "nodes", ""],
    queryFn: () => api<{ rows: Row[] }>("/api/v1/resources/nodes"),
  });
  const row = health.data?.rows?.[0] ?? {};
  const report = compliance.data?.rows?.[0] ?? {};
  const chart = useMemo(() => {
    const counts = new Map<string, number>();
    for (const vm of vms.data?.rows ?? []) {
      const state = text(vm.state) || "unknown";
      counts.set(state, (counts.get(state) ?? 0) + 1);
    }
    return [...counts.entries()].map(([state, count]) => ({ state, count }));
  }, [vms.data]);
  const slices = chart.map((item, index) => ({
    label: item.state,
    value: item.count,
    color: toneColor(toneFor(item.state)) === "#6366f1" ? STATE_COLORS[index % STATE_COLORS.length] : toneColor(toneFor(item.state)),
  }));
  const running = chart.find((item) => item.state === "running")?.count;
  const status = text(row.status);
  const conditions = Array.isArray(row.conditions) ? row.conditions.map((item) => text(item)).filter(Boolean) : [];
  const capacity = (nodes.data?.rows ?? [])
    .map((node) => ({
      name: text(node.hostname) || text(node.name) || text(node.id),
      cpu: usedPercent(node.cpu_used, node.cpu),
      memory: usedPercent(node.memory_used, node.memory),
    }))
    .filter((node) => node.name && (node.cpu !== null || node.memory !== null))
    .slice(0, 6);
  const error = health.error ?? vms.error;
  const loading = !error && (health.isLoading || vms.isLoading);

  return (
    <div className="stack">
      <header className="top">
        <div>
          <p className="eyebrow">Cluster</p>
          <h1>Overview</h1>
        </div>
      </header>
      {error ? <p className="error">{(error as Error).message}</p> : null}
      {loading ? <Skeleton /> : null}
      {status || text(row.headline) ? (
        <div className={`banner ${toneFor(status)}`}>
          {status ? <span className={`pill ${toneFor(status)}`}>{status}</span> : null}
          <strong>{text(row.headline) || "Cluster"}</strong>
        </div>
      ) : null}
      <div className="grid">
        <div className="card">
          <span className="muted">Ready nodes</span>
          <strong className="metric">{text(row.ready_nodes) || "—"}</strong>
        </div>
        <div className="card">
          <span className="muted">Virtual machines</span>
          <strong className="metric">{text(row.vm_count) || String(vms.data?.rows.length ?? "—")}</strong>
        </div>
        <div className="card">
          <span className="muted">Running</span>
          <strong className="metric">{(running ?? text(report.running_vms)) || "—"}</strong>
        </div>
        <div className="card">
          <span className="muted">Not ready</span>
          <strong className="metric">{text(row.not_ready_nodes) || "0"}</strong>
        </div>
      </div>
      <div className="split">
        <section className="card stack">
          <h2>Virtual machines</h2>
          <Donut slices={slices} />
        </section>
        <section className="card stack">
          <h2>By state</h2>
          <Bars label="Virtual machines by state" items={chart.map((item) => ({ label: item.state, value: item.count, color: toneColor(toneFor(item.state)) }))} />
        </section>
      </div>
      {capacity.length ? (
        <section className="card stack">
          <h2>Node capacity</h2>
          {capacity.map((node) => (
            <div key={node.name} className="stack">
              <strong>{node.name}</strong>
              {node.cpu !== null ? <Meter label="CPU" percent={node.cpu} /> : null}
              {node.memory !== null ? <Meter label="Memory" percent={node.memory} /> : null}
            </div>
          ))}
        </section>
      ) : null}
      {conditions.length ? (
        <section className="card stack">
          <h2>Conditions</h2>
          <ul className="alerts">
            {conditions.map((condition) => (
              <li key={condition}>{condition}</li>
            ))}
          </ul>
        </section>
      ) : null}
      <div className="quick">
        <Link to="/r/$id" params={{ id: "vms" }} search={{ q: "" }}>Virtual machines</Link>
        <Link to="/r/$id" params={{ id: "nodes" }} search={{ q: "" }}>Nodes</Link>
        <Link to="/r/$id" params={{ id: "health" }} search={{ q: "" }}>Health</Link>
        <Link to="/r/$id" params={{ id: "storage" }} search={{ q: "" }}>Host storage</Link>
        <Link to="/r/$id" params={{ id: "audit" }} search={{ q: "" }}>Audit</Link>
      </div>
    </div>
  );
}

function ListField({ field, value, onChange }: { field: Field; value: string | boolean | undefined; onChange: (value: string) => void }) {
  const columns = LIST_COLUMNS[field.name] ?? [{ name: "value" }];
  const rows = parseList(value);
  function write(next: Record<string, string | boolean>[]) {
    onChange(JSON.stringify(next));
  }
  return (
    <div className="list-field">
      <span>
        {field.label}
        {field.required ? " *" : ""}
      </span>
      {rows.map((row, index) => (
        <div className="list-row" key={`${field.name}-${index}`}>
          {columns.map((column) =>
            column.kind === "bool" ? (
              <label key={column.name}>
                {column.name}
                <input
                  type="checkbox"
                  checked={Boolean(row[column.name])}
                  onChange={(event) => {
                    const next = rows.map((item, itemIndex) => (itemIndex === index ? { ...item, [column.name]: event.target.checked } : item));
                    write(next);
                  }}
                />
              </label>
            ) : (
              <input
                key={column.name}
                placeholder={column.name}
                type={column.kind === "number" ? "number" : "text"}
                value={String(row[column.name] ?? "")}
                onChange={(event) => {
                  const next = rows.map((item, itemIndex) => (itemIndex === index ? { ...item, [column.name]: event.target.value } : item));
                  write(next);
                }}
              />
            ),
          )}
          <button type="button" onClick={() => write(rows.filter((_, itemIndex) => itemIndex !== index))}>
            Remove
          </button>
        </div>
      ))}
      <button type="button" onClick={() => write([...rows, {}])}>
        Add
      </button>
    </div>
  );
}

function Fields({ fields, values, onChange }: { fields: Field[]; values: Record<string, string | boolean>; onChange: (name: string, value: string | boolean) => void }) {
  return (
    <>
      {fields.map((field) =>
        field.kind === "json" ? (
          <ListField key={field.name} field={field} value={values[field.name]} onChange={(value) => onChange(field.name, value)} />
        ) : (
          <label key={field.name}>
            {field.label}
            {field.required ? " *" : ""}
            {field.kind === "bool" ? (
              <input type="checkbox" checked={Boolean(values[field.name])} onChange={(event) => onChange(field.name, event.target.checked)} />
            ) : field.name.includes("nix") || field.name.endsWith("_pem") || field.name === "public_key" || field.name === "cloud_init_user_data" || field.name === "sqlite_base64" ? (
              <textarea value={String(values[field.name] ?? "")} onChange={(event) => onChange(field.name, event.target.value)} />
            ) : (
              <input
                type={field.kind === "number" ? "number" : "text"}
                value={String(values[field.name] ?? "")}
                required={field.required}
                onChange={(event) => onChange(field.name, event.target.value)}
              />
            )}
          </label>
        ),
      )}
    </>
  );
}

type FormRequest = {
  title: string;
  fields: Field[];
  checks: Action[];
  preload: boolean;
  confirm: boolean;
  requireForce: boolean;
  resourceId: string;
  rowKey: string;
  hint: string;
  submit: (form: Record<string, string | boolean>) => Promise<void>;
};

function useShownColumns(id: string, rows: Row[]) {
  const defs = useMemo(() => columnsFor(id, rows), [id, rows]);
  const [picked, setPicked] = useState<string[] | null>(null);
  useEffect(() => {
    setPicked(null);
  }, [id]);
  const defaults = defs.filter((column) => !column.hidden).map((column) => column.key);
  const shown = (picked ?? defaults).filter((key) => defs.some((column) => column.key === key));
  const visible = shown.length ? shown : defaults;
  function toggle(key: string) {
    const next = new Set(visible);
    if (next.has(key)) {
      if (next.size === 1) return;
      next.delete(key);
    } else next.add(key);
    setPicked(defs.filter((column) => next.has(column.key)).map((column) => column.key));
  }
  return { defs, shown: visible, toggle };
}

export function ResourcePage() {
  const { id } = useParams({ from: "/r/$id" });
  const { q } = useSearch({ from: "/r/$id" });
  const queryClient = useQueryClient();
  const catalog = useQuery({ queryKey: ["catalog"], queryFn: () => api<Catalog>("/api/v1/catalog") });
  const resource = catalog.data?.resources.find((item) => item.id === id);
  const [volume, setVolume] = useState("");
  const [filter, setFilter] = useState(q);
  const [since, setSince] = useState("");
  const [auditAction, setAuditAction] = useState("");
  const [nodeId, setNodeId] = useState("");
  const deferredSince = useDeferredValue(since);
  const deferredAction = useDeferredValue(auditAction);
  const deferredNode = useDeferredValue(nodeId);
  const [request, setRequest] = useState<FormRequest | null>(null);
  const [drawer, setDrawer] = useState<{ row: Row; body: unknown; loading: boolean; error: string } | null>(null);
  const [peek, setPeek] = useState<{ title: string; rows: Row[] } | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const toastId = useRef(1);

  useEffect(() => {
    setFilter(q);
    setVolume("");
    setSince("");
    setAuditAction("");
    setNodeId("");
    setRequest(null);
    setDrawer(null);
    setPeek(null);
  }, [id, q]);

  function push(toast: Omit<Toast, "id">) {
    const nextId = toastId.current++;
    setToasts((current) => [...current.slice(-3), { ...toast, id: nextId }]);
    window.setTimeout(() => setToasts((current) => current.filter((item) => item.id !== nextId)), 8000);
  }

  const listKey = id === "snapshots" ? volume : id === "audit" ? [deferredSince, deferredAction].filter(Boolean).join("|") : id === "gpus" ? deferredNode : "";
  const rows = useQuery({
    queryKey: ["rows", id, listKey],
    enabled: Boolean(resource?.list),
    refetchInterval: id === "operations" ? 5_000 : false,
    queryFn: () => {
      const params = new URLSearchParams();
      if (id === "snapshots" && volume) params.set("volume", volume);
      if (id === "audit" && deferredSince) params.set("since", deferredSince);
      if (id === "audit" && deferredAction) params.set("action", deferredAction);
      if (id === "gpus" && deferredNode) params.set("node_id", deferredNode);
      const query = params.toString();
      return api<{ rows: Row[] }>(`/api/v1/resources/${id}${query ? `?${query}` : ""}`);
    },
  });

  const indexed = useMemo(() => (rows.data?.rows ?? []).map((row) => ({ row, haystack: haystack(row) })), [rows.data]);
  const visible = useMemo(() => {
    const query = filter.trim().toLowerCase();
    if (!query) return indexed.map((item) => item.row);
    return indexed.filter((item) => item.haystack.includes(query)).map((item) => item.row);
  }, [indexed, filter]);
  const { defs, shown, toggle } = useShownColumns(id, visible);
  const board = visible.length === 1 && (rows.data?.rows.length ?? 0) === 1;

  async function run(action: Action, row?: Row, form: Record<string, string | boolean> = {}) {
    if (!resource) return;
    try {
      const idValue = row ? text(row[resource.idField]) : text(form.name);
      const result = await api<unknown>(`/api/v1/resources/${resource.id}/actions`, {
        method: "POST",
        body: JSON.stringify({
          action: action.name,
          id: idValue,
          confirm: action.confirm || Boolean(form.confirm),
          extra: { ...(row ?? {}), ...payload(action.fields, form) },
        }),
      });
      push({ tone: "ok", title: `${action.label} sent`, detail: detailOf(result) });
      await queryClient.invalidateQueries({ queryKey: ["rows", resource.id] });
    } catch (error) {
      push({ tone: "bad", title: (error as Error).message });
    }
  }

  async function create(form: Record<string, string | boolean>) {
    if (!resource) return;
    try {
      const result = await api<unknown>(`/api/v1/resources/${resource.id}`, {
        method: "POST",
        body: JSON.stringify(payload(resource.fields, form)),
      });
      push({ tone: "ok", title: `${displayTitle(resource.id, resource.title)} created`, detail: detailOf(result) });
      await queryClient.invalidateQueries({ queryKey: ["rows", resource.id] });
    } catch (error) {
      push({ tone: "bad", title: (error as Error).message });
    }
  }

  function openForm(
    title: string,
    fields: Field[],
    submit: (form: Record<string, string | boolean>) => Promise<void>,
    options: { checks?: Action[]; preload?: boolean; confirm?: boolean; requireForce?: boolean; row?: Row } = {},
  ) {
    if (!resource) return;
    setRequest({
      title,
      fields,
      checks: options.checks ?? [],
      preload: Boolean(options.preload),
      confirm: Boolean(options.confirm),
      requireForce: Boolean(options.requireForce),
      resourceId: resource.id,
      rowKey: options.row ? text(options.row[resource.idField]) : "",
      hint: options.row ? rowTitle(options.row) : "",
      submit,
    });
  }

  function openDrawer(row: Row) {
    if (!resource) return;
    setDrawer({ row, body: null, loading: Boolean(resource.inspect), error: "" });
    if (!resource.inspect) return;
    const key = text(row[resource.idField]);
    if (!key) {
      setDrawer({ row, body: null, loading: false, error: "This row has no id." });
      return;
    }
    api<unknown>(`/api/v1/resources/${resource.id}/items/${encodeURIComponent(key)}`)
      .then((body) => setDrawer((current) => (current && current.row === row ? { ...current, body, loading: false } : current)))
      .catch((error: Error) => setDrawer((current) => (current && current.row === row ? { ...current, loading: false, error: error.message } : current)));
  }

  if (!resource && catalog.isSuccess) return <p className="error">Unknown resource.</p>;

  const actions = (row?: Row) =>
    resource ? <RowActions resource={resource} row={row} onOpen={openForm} onRun={run} onInspect={openDrawer} /> : null;
  const updated = rows.dataUpdatedAt ? formatDate(new Date(rows.dataUpdatedAt).toISOString()) : "";

  return (
    <div className="stack">
      <header className="top">
        <div>
          {resource ? <p className="eyebrow">{resource.group}</p> : null}
          <h1>{resource ? displayTitle(resource.id, resource.title) : id}</h1>
          {resource ? (
            <p className="muted">
              {resource.kctl ? `kctl ${resource.kctl}` : ""}
              {resource.terraform.length ? ` · ${resource.terraform.join(", ")}` : ""}
              {updated ? ` · updated ${updated}` : ""}
            </p>
          ) : null}
        </div>
        {resource?.list ? (
          <button type="button" onClick={() => rows.refetch()} disabled={rows.isFetching}>
            Refresh
          </button>
        ) : null}
      </header>
      <div className="toolbar">
        {resource?.list && !board ? (
          <input placeholder="Filter" aria-label="Filter" value={filter} onChange={(event) => setFilter(event.target.value)} />
        ) : null}
        {id === "snapshots" ? (
          <input placeholder="Volume" aria-label="Volume" value={volume} onChange={(event) => setVolume(event.target.value)} />
        ) : null}
        {id === "audit" ? (
          <>
            <input placeholder="Since" aria-label="Since" value={since} onChange={(event) => setSince(event.target.value)} />
            <input placeholder="Action" aria-label="Audit action" value={auditAction} onChange={(event) => setAuditAction(event.target.value)} />
          </>
        ) : null}
        {id === "gpus" ? (
          <input placeholder="Node" aria-label="Node" value={nodeId} onChange={(event) => setNodeId(event.target.value)} />
        ) : null}
        {resource?.creatable ? (
          <button
            type="button"
            className="primary"
            onClick={() =>
              openForm(`Create ${displayTitle(resource.id, resource.title)}`, resource.fields, create, {
                checks: resource.actions.filter((action) => action.name === "plan" || action.name === "classify"),
              })
            }
          >
            Create
          </button>
        ) : null}
        {resource && !resource.list
          ? resource.actions.map((action) =>
              action.client ? null : (
                <button
                  type="button"
                  key={action.name}
                  className={action.confirm ? "danger" : ""}
                  onClick={() => openForm(action.label, action.fields, (form) => run(action, undefined, form), { confirm: action.confirm })}
                >
                  {action.label}
                </button>
              ),
            )
          : null}
        {board && visible[0] ? actions(visible[0]) : null}
        {resource?.list && !board && defs.length ? <ColumnChooser columns={defs} shown={shown} onToggle={toggle} /> : null}
      </div>
      {catalog.isLoading || (resource?.list && rows.isLoading) ? <Skeleton /> : null}
      {rows.isError ? <p className="error">{(rows.error as Error).message}</p> : null}
      {resource && !resource.list ? (
        <Empty title="No inventory for this resource" body="Create it here, then delete it by name. The controller does not return a list." />
      ) : null}
      {resource?.list && rows.isSuccess ? (
        board && visible[0] ? (
          <SummaryPage id={id} row={visible[0]} onPeek={(title, nested) => setPeek({ title, rows: nested })} />
        ) : (
          <DataTable
            rows={visible}
            columns={defs}
            shown={shown}
            resetKey={`${id}:${filter}`}
            onPeek={(title, nested) => setPeek({ title, rows: nested })}
            onRow={openDrawer}
            trailing={resource.inspect || resource.actions.length ? (row) => actions(row) : undefined}
          />
        )
      ) : null}
      <Toasts items={toasts} onClose={(toast) => setToasts((current) => current.filter((item) => item.id !== toast))} />
      <PeekDialog peek={peek} onClose={() => setPeek(null)} />
      <DetailDrawer
        open={drawer !== null}
        title={drawer ? rowTitle(drawer.row) : "Details"}
        row={drawer?.row ?? null}
        body={drawer?.body}
        loading={Boolean(drawer?.loading)}
        error={drawer?.error ?? ""}
        actions={drawer ? actions(drawer.row) : null}
        onClose={() => setDrawer(null)}
      />
      <FormDialog request={request} onClose={() => setRequest(null)} onError={(message) => push({ tone: "bad", title: message })} />
    </div>
  );
}

function FormDialog({ request, onClose, onError }: { request: FormRequest | null; onClose: () => void; onError: (message: string) => void }) {
  const [values, setValues] = useState<Record<string, string | boolean>>({});
  const [note, setNote] = useState("");
  const [ready, setReady] = useState(true);
  const [loadError, setLoadError] = useState("");
  const fields = request?.fields ?? [];
  const essential = fields.filter((field) => ESSENTIAL.has(field.name));
  const rest = fields.filter((field) => !ESSENTIAL.has(field.name));
  const split = fields.length > 8 && essential.length > 0;
  const blocked = Boolean(request && ((request.requireForce && !values.force) || (request.preload && !ready)));

  useEffect(() => {
    if (!request) return;
    setValues({});
    setNote("");
    setLoadError("");
    if (!request.preload) {
      setReady(true);
      return;
    }
    let cancel = false;
    setReady(false);
    api<{ spec?: Record<string, unknown> }>(`/api/v1/resources/${request.resourceId}/items/${encodeURIComponent(request.rowKey)}`)
      .then((body) => {
        if (cancel) return;
        if (!body.spec) {
          setLoadError("The controller returned no spec. Update was not loaded.");
          return;
        }
        setValues(specToValues(request.fields, body.spec));
        setReady(true);
      })
      .catch((error: Error) => {
        if (!cancel) setLoadError(error.message);
      });
    return () => {
      cancel = true;
    };
  }, [request]);

  async function runCheck(action: Action) {
    if (!request) return;
    try {
      const result = await api<unknown>(`/api/v1/resources/${request.resourceId}/actions`, {
        method: "POST",
        body: JSON.stringify({
          action: action.name,
          id: text(values.name),
          confirm: Boolean(values.confirm),
          extra: payload(request.fields, values),
        }),
      });
      setNote(pretty(result));
    } catch (error) {
      setNote((error as Error).message);
    }
  }

  return (
    <Dialog.Root open={request !== null} onOpenChange={(open) => { if (!open) onClose(); }}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className={fields.some((field) => field.kind === "json") || split ? "dialog wide" : "dialog"}>
          <Dialog.Title>{request?.title ?? ""}</Dialog.Title>
          {request?.confirm ? (
            <p>{request.hint ? `${request.title} ${request.hint}.` : `${request.title}.`} This changes the cluster.</p>
          ) : null}
          {request?.requireForce ? (
            <p>Force clears a stranded receive session on the named node. The controller refuses the node that owns the running VM.</p>
          ) : null}
          {request?.preload ? <p>Loaded from the current cluster spec. Apply sends this form back as the replacement.</p> : null}
          {loadError ? <p className="error">{loadError}</p> : null}
          <Fields fields={split ? essential : fields} values={values} onChange={(name, value) => setValues((current) => ({ ...current, [name]: value }))} />
          {split ? (
            <details className="advanced">
              <summary>Advanced</summary>
              <Fields fields={rest} values={values} onChange={(name, value) => setValues((current) => ({ ...current, [name]: value }))} />
            </details>
          ) : null}
          {note ? <pre className="code">{note}</pre> : null}
          <div className="buttons">
            {request?.checks.map((action) => (
              <button key={action.name} type="button" onClick={() => runCheck(action)}>
                {action.label}
              </button>
            ))}
            <Dialog.Close asChild>
              <button type="button">Cancel</button>
            </Dialog.Close>
            <button
              type="button"
              className="primary"
              disabled={blocked}
              onClick={() => {
                if (!request || blocked) return;
                const pending = request.submit;
                const current = values;
                onClose();
                pending(current).catch((error: Error) => onError(error.message));
              }}
            >
              Apply
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
