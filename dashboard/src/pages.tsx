import { useDeferredValue, useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import * as Dialog from "@radix-ui/react-dialog";
import { fileToBase64, findResource, isDownload, isReveal, type Action, type Field, type Resource, type Reveal } from "./catalog";
import type { Values } from "./forms";
import { errorMessage } from "./rpc";
import type { Row } from "./rows";
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

type FormValues = Record<string, string | boolean>;

const ESSENTIAL = new Set(["name", "cpu", "memory_bytes", "storage_backend", "storage_size_bytes", "image_url", "image_sha256", "image_path", "desired_state"]);

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

function parseList(raw: string | boolean | undefined): FormValues[] {
  if (typeof raw !== "string" || !raw.trim()) return [];
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((item) => item && typeof item === "object") as FormValues[];
  } catch {
    return [];
  }
}

function specToValues(fields: Field[], spec: Values): FormValues {
  const values: FormValues = {};
  for (const field of fields) {
    const raw = spec[field.name];
    if (raw === undefined || raw === null) continue;
    if (field.kind === "bool") values[field.name] = Boolean(raw);
    else if (field.kind === "json") values[field.name] = JSON.stringify(raw);
    else if (Array.isArray(raw)) values[field.name] = raw.join(", ");
    else values[field.name] = String(raw);
  }
  return values;
}

function payload(fields: Field[], values: FormValues): Values {
  const body: Values = {};
  for (const field of fields) {
    const raw = values[field.name];
    if (raw === undefined || raw === "") continue;
    if (field.kind === "bool") body[field.name] = Boolean(raw);
    else if (field.kind === "json") {
      const source = String(raw).trim();
      if (!source) continue;
      try {
        body[field.name] = JSON.parse(source);
      } catch {
        throw new Error(`${field.label} must be a JSON list`);
      }
    } else body[field.name] = raw;
  }
  return body;
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
    const keys = Object.keys(record).filter((key) => key !== "ok");
    if (!keys.length) return undefined;
    if (keys.length === 1 && typeof record[keys[0]] === "string") return String(record[keys[0]]);
  }
  return pretty(value);
}

function saveFile(name: string, data: Uint8Array, type: string) {
  const blob = new Blob([data as BlobPart], { type });
  const link = document.createElement("a");
  link.href = URL.createObjectURL(blob);
  link.download = name;
  link.click();
  window.setTimeout(() => URL.revokeObjectURL(link.href), 1_000);
}

function useRows(resource: Resource | undefined, filters: Values) {
  return useQuery({
    queryKey: ["rows", resource?.id ?? "", JSON.stringify(filters)],
    enabled: Boolean(resource?.list),
    refetchInterval: resource?.refreshMs ?? false,
    queryFn: () => resource!.list!(filters),
  });
}

const STATE_COLORS = ["#10b981", "#f59e0b", "#f87171", "#6366f1", "#8b5cf6", "#3b82f6"];
const NO_FILTERS: Values = {};

export function Overview() {
  const health = useRows(findResource("health"), NO_FILTERS);
  const vms = useRows(findResource("vms"), NO_FILTERS);
  const nodes = useRows(findResource("nodes"), NO_FILTERS);
  const updates = useRows(findResource("cluster-updates"), NO_FILTERS);
  const row = health.data?.[0] ?? {};
  const chart = useMemo(() => {
    const counts = new Map<string, number>();
    for (const vm of vms.data ?? []) {
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
  const capacity = (nodes.data ?? [])
    .map((node) => ({
      name: text(node.hostname) || text(node.node_id),
      cpu: usedPercent(node.cpu_cores_used, node.cpu_cores),
      memory: usedPercent(node.memory_bytes_used, node.memory_bytes),
    }))
    .filter((node) => node.name && (node.cpu !== null || node.memory !== null))
    .slice(0, 6);
  const active = (updates.data ?? []).filter((update) => !["succeeded", "cancelled", "rolled-back", "failed"].includes(text(update.phase)));
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
      {error ? <p className="error">{errorMessage(error)}</p> : null}
      {loading ? <Skeleton /> : null}
      {status ? (
        <div className={`banner ${toneFor(status)}`}>
          <span className={`pill ${toneFor(status)}`}>{status}</span>
          <strong>Cluster</strong>
          {text(row.failover_capacity) ? <span className="muted">Failover capacity {text(row.failover_capacity)}</span> : null}
        </div>
      ) : null}
      <div className="grid">
        <div className="card">
          <span className="muted">Ready nodes</span>
          <strong className="metric">{text(row.ready_nodes) || "—"}</strong>
        </div>
        <div className="card">
          <span className="muted">Virtual machines</span>
          <strong className="metric">{text(row.vm_count) || String(vms.data?.length ?? "—")}</strong>
        </div>
        <div className="card">
          <span className="muted">Running</span>
          <strong className="metric">{running ?? "0"}</strong>
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
      {active.length ? (
        <section className="card stack">
          <h2>Updates in progress</h2>
          <ul className="alerts">
            {active.map((update) => (
              <li key={text(update.name)}>
                <Link to="/r/$id" params={{ id: "cluster-updates" }} search={{ q: text(update.name) }}>
                  {text(update.name)}
                </Link>{" "}
                {text(update.version)} · {text(update.phase)} · approval {text(update.approval_status)}
              </li>
            ))}
          </ul>
        </section>
      ) : null}
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
  function write(next: FormValues[]) {
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
                  onChange={(event) => write(rows.map((item, itemIndex) => (itemIndex === index ? { ...item, [column.name]: event.target.checked } : item)))}
                />
              </label>
            ) : (
              <input
                key={column.name}
                placeholder={column.name}
                aria-label={`${field.label} ${index + 1} ${column.name}`}
                type={column.kind === "number" ? "number" : "text"}
                value={String(row[column.name] ?? "")}
                onChange={(event) => write(rows.map((item, itemIndex) => (itemIndex === index ? { ...item, [column.name]: event.target.value } : item)))}
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

function FileField({ field, onChange }: { field: Field; onChange: (value: string) => void }) {
  const [name, setName] = useState("");
  return (
    <label>
      {field.label}
      {field.required ? " *" : ""}
      <input
        type="file"
        onChange={async (event) => {
          const picked = event.target.files?.[0];
          if (!picked) return;
          setName(`${picked.name} (${picked.size.toLocaleString("en-GB")} bytes)`);
          onChange(await fileToBase64(picked));
        }}
      />
      {name ? <span className="muted">{name}</span> : null}
    </label>
  );
}

function Fields({ fields, values, onChange }: { fields: Field[]; values: FormValues; onChange: (name: string, value: string | boolean) => void }) {
  return (
    <>
      {fields.map((field) => {
        if (field.kind === "json") return <ListField key={field.name} field={field} value={values[field.name]} onChange={(value) => onChange(field.name, value)} />;
        if (field.kind === "file") return <FileField key={field.name} field={field} onChange={(value) => onChange(field.name, value)} />;
        return (
          <label key={field.name} className={field.kind === "bool" ? "check" : undefined}>
            {field.kind === "bool" ? <input type="checkbox" checked={Boolean(values[field.name])} onChange={(event) => onChange(field.name, event.target.checked)} /> : null}
            {field.label}
            {field.required ? " *" : ""}
            {field.kind === "multiline" ? (
              <textarea value={String(values[field.name] ?? "")} placeholder={field.placeholder} onChange={(event) => onChange(field.name, event.target.value)} />
            ) : field.kind !== "bool" ? (
              <input
                type={field.kind === "number" ? "number" : field.kind === "secret" ? "password" : "text"}
                autoComplete={field.kind === "secret" ? "new-password" : "off"}
                placeholder={field.placeholder}
                value={String(values[field.name] ?? "")}
                required={field.required}
                onChange={(event) => onChange(field.name, event.target.value)}
              />
            ) : null}
          </label>
        );
      })}
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
  resource: Resource;
  rowKey: string;
  hint: string;
  submit: (form: FormValues) => Promise<void>;
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
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const resource = findResource(id);
  const [filter, setFilter] = useState(q);
  const [filters, setFilters] = useState<Values>({});
  const deferredFilters = useDeferredValue(filters);
  const [request, setRequest] = useState<FormRequest | null>(null);
  const [drawer, setDrawer] = useState<{ row: Row; body: unknown; loading: boolean; error: string } | null>(null);
  const [peek, setPeek] = useState<{ title: string; rows: Row[] } | null>(null);
  const [reveal, setReveal] = useState<{ title: string; value: Reveal } | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const toastId = useRef(1);

  useEffect(() => {
    setFilter(q);
    setFilters({});
    setRequest(null);
    setDrawer(null);
    setPeek(null);
  }, [id, q]);

  function push(toast: Omit<Toast, "id">) {
    const nextId = toastId.current++;
    setToasts((current) => [...current.slice(-3), { ...toast, id: nextId }]);
    window.setTimeout(() => setToasts((current) => current.filter((item) => item.id !== nextId)), 8000);
  }

  const rows = useRows(resource, deferredFilters);
  const indexed = useMemo(() => (rows.data ?? []).map((row) => ({ row, haystack: haystack(row) })), [rows.data]);
  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return indexed.map((item) => item.row);
    return indexed.filter((item) => item.haystack.includes(needle)).map((item) => item.row);
  }, [indexed, filter]);
  const { defs, shown, toggle } = useShownColumns(id, visible);
  const board = Boolean(resource?.single) && visible.length === 1;

  function settle(title: string, result: unknown) {
    if (isDownload(result)) {
      saveFile(result.download.filename, result.download.data, result.download.type);
      push({ tone: "ok", title: `${title}: saved ${result.download.filename}` });
      return;
    }
    if (isReveal(result)) {
      setReveal({ title, value: result });
      return;
    }
    push({ tone: "ok", title: `${title} sent`, detail: detailOf(result) });
  }

  async function run(action: Action, row?: Row, form: FormValues = {}) {
    if (!resource) return;
    if (action.console && row) {
      navigate({ to: "/console/$vm", params: { vm: text(row.name) || text(row[resource.idField]) } });
      return;
    }
    try {
      const result = await action.run({ ...(row ?? {}), ...payload(action.fields, form) });
      settle(action.label, result);
      await queryClient.invalidateQueries({ queryKey: ["rows", resource.id] });
    } catch (error) {
      push({ tone: "bad", title: `${action.label} failed`, detail: errorMessage(error) });
    }
  }

  async function create(form: FormValues) {
    if (!resource?.create) return;
    const title = `Create ${displayTitle(resource.id, resource.title)}`;
    try {
      settle(title, await resource.create(payload(resource.fields, form)));
      await queryClient.invalidateQueries({ queryKey: ["rows", resource.id] });
    } catch (error) {
      push({ tone: "bad", title: `${title} failed`, detail: errorMessage(error) });
    }
  }

  function openForm(
    title: string,
    fields: Field[],
    submit: (form: FormValues) => Promise<void>,
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
      resource,
      rowKey: options.row ? text(options.row[resource.idField]) : "",
      hint: options.row ? rowTitle(options.row) : "",
      submit,
    });
  }

  function openDrawer(row: Row) {
    if (!resource) return;
    const inspect = resource.inspect;
    setDrawer({ row, body: null, loading: Boolean(inspect), error: "" });
    if (!inspect) return;
    const key = text(row[resource.idField]);
    if (!key) {
      setDrawer({ row, body: null, loading: false, error: "This row has no id." });
      return;
    }
    inspect(key)
      .then((body) => setDrawer((current) => (current && current.row === row ? { ...current, body, loading: false } : current)))
      .catch((error: unknown) => setDrawer((current) => (current && current.row === row ? { ...current, loading: false, error: errorMessage(error) } : current)));
  }

  if (!resource) return <Empty title="Unknown page" body={`There is no resource called ${id}.`} />;

  const actions = (row?: Row) => <RowActions resource={resource} row={row} onOpen={openForm} onRun={run} onInspect={openDrawer} />;
  const updated = rows.dataUpdatedAt ? formatDate(new Date(rows.dataUpdatedAt).toISOString()) : "";

  return (
    <div className="stack">
      <header className="top">
        <div>
          <p className="eyebrow">{resource.group}</p>
          <h1>{displayTitle(resource.id, resource.title)}</h1>
          <p className="muted">
            {resource.kctl ? `kctl ${resource.kctl}` : ""}
            {resource.terraform.length ? ` · ${resource.terraform.join(", ")}` : ""}
            {updated ? ` · updated ${updated}` : ""}
          </p>
        </div>
        <button type="button" onClick={() => rows.refetch()} disabled={rows.isFetching}>
          Refresh
        </button>
      </header>
      <div className="toolbar">
        {!board ? <input placeholder="Filter" aria-label="Filter" value={filter} onChange={(event) => setFilter(event.target.value)} /> : null}
        {resource.filters?.map((item) =>
          item.kind === "bool" ? (
            <label key={item.name} className="check">
              <input type="checkbox" checked={Boolean(filters[item.name])} onChange={(event) => setFilters((current) => ({ ...current, [item.name]: event.target.checked }))} />
              {item.label}
            </label>
          ) : (
            <input
              key={item.name}
              placeholder={item.placeholder ?? item.label}
              aria-label={item.label}
              value={String(filters[item.name] ?? "")}
              onChange={(event) => setFilters((current) => ({ ...current, [item.name]: event.target.value }))}
            />
          ),
        )}
        {resource.create ? (
          <button
            type="button"
            className="primary"
            onClick={() =>
              openForm(`Create ${displayTitle(resource.id, resource.title)}`, resource.fields, create, {
                checks: resource.actions.filter((action) => resource.checks?.includes(action.name)),
              })
            }
          >
            Create
          </button>
        ) : null}
        {board && visible[0] ? actions(visible[0]) : null}
        {!board && defs.length ? <ColumnChooser columns={defs} shown={shown} onToggle={toggle} /> : null}
      </div>
      {rows.isLoading ? <Skeleton /> : null}
      {rows.isError ? <p className="error">{errorMessage(rows.error)}</p> : null}
      {rows.isSuccess ? (
        board && visible[0] ? (
          <SummaryPage id={id} row={visible[0]} onPeek={(title, nested) => setPeek({ title, rows: nested })} />
        ) : visible.length === 0 ? (
          <Empty title={`No ${displayTitle(resource.id, resource.title).toLowerCase()}`} body={resource.create ? "Create one with the button above." : "The controller returned no rows."} />
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
      <RevealDialog reveal={reveal} onClose={() => setReveal(null)} />
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

function RevealDialog({ reveal, onClose }: { reveal: { title: string; value: Reveal } | null; onClose: () => void }) {
  return (
    <Dialog.Root open={reveal !== null} onOpenChange={(open) => { if (!open) onClose(); }}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className="dialog wide">
          <Dialog.Title>{reveal?.title ?? ""}</Dialog.Title>
          {reveal?.value.note ? <p className="banner warn">{reveal.value.note}</p> : null}
          {Object.entries(reveal?.value.reveal ?? {}).map(([key, value]) => (
            <section key={key} className="stack">
              <div className="row-head">
                <strong>{key}</strong>
                <button type="button" onClick={() => navigator.clipboard?.writeText(value)}>
                  Copy
                </button>
              </div>
              <pre className="code">{value || "—"}</pre>
            </section>
          ))}
          <div className="buttons">
            <Dialog.Close asChild>
              <button type="button" className="primary">
                Done
              </button>
            </Dialog.Close>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function FormDialog({ request, onClose, onError }: { request: FormRequest | null; onClose: () => void; onError: (message: string) => void }) {
  const [values, setValues] = useState<FormValues>({});
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
    const inspect = request.resource.inspect;
    if (!inspect) {
      setLoadError("This resource has no stored spec to load.");
      return;
    }
    inspect(request.rowKey)
      .then((body) => {
        if (cancel) return;
        if (!body.spec) {
          setLoadError("The controller returned no spec. Update was not loaded.");
          return;
        }
        setValues(specToValues(request.fields, body.spec));
        setReady(true);
      })
      .catch((error: unknown) => {
        if (!cancel) setLoadError(errorMessage(error));
      });
    return () => {
      cancel = true;
    };
  }, [request]);

  async function runCheck(action: Action) {
    if (!request) return;
    try {
      setNote(pretty(await action.run(payload(request.fields, values))));
    } catch (error) {
      setNote(errorMessage(error));
    }
  }

  return (
    <Dialog.Root open={request !== null} onOpenChange={(open) => { if (!open) onClose(); }}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className={fields.some((field) => field.kind === "json" || field.kind === "multiline") || split ? "dialog wide" : "dialog"}>
          <Dialog.Title>{request?.title ?? ""}</Dialog.Title>
          {request?.confirm ? <p>{request.hint ? `${request.title} ${request.hint}.` : `${request.title}.`} This changes the cluster.</p> : null}
          {request?.requireForce ? <p>Force clears a stranded receive session on the named node. The controller refuses the node that owns the running VM.</p> : null}
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
              className={request?.confirm ? "danger" : "primary"}
              disabled={blocked}
              onClick={() => {
                if (!request || blocked) return;
                const pending = request.submit;
                const current = values;
                onClose();
                pending(current).catch((error: unknown) => onError(errorMessage(error)));
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
