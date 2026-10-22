import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";
import * as Dialog from "@radix-ui/react-dialog";
import { api, payload, specToValues, type Action, type Catalog, type Field, type Resource, type Row } from "./api";

const PAGE_SIZE = 25;
const COLUMN_LIMIT = 8;
const PREFERRED_COLUMNS = ["name", "id", "title", "status", "state", "phase", "kind", "node_id"];

function cell(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") return "";
  return String(value);
}

function isRecord(value: unknown): value is Row {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function isObjectArray(value: unknown): value is Row[] {
  return Array.isArray(value) && value.length > 0 && value.every(isRecord);
}

/** Scalar text for a table cell. Nested objects stay summarized so a disk
 * inventory cannot stringify into a multi-megabyte DOM node. */
function cellText(value: unknown): string {
  if (value === null || value === undefined || value === "") return "";
  if (typeof value === "string") return value.length > 180 ? `${value.slice(0, 177)}…` : value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (Array.isArray(value)) {
    if (value.length === 0) return "";
    if (value.every((item) => item == null || typeof item !== "object")) {
      const text = value
        .filter((item) => item != null && item !== "")
        .map((item) => String(item))
        .join(", ");
      return text.length > 180 ? `${text.slice(0, 177)}…` : text;
    }
    return String(value.length);
  }
  if (isRecord(value)) {
    const parts = Object.entries(value)
      .filter(([, item]) => item != null && item !== "" && typeof item !== "object")
      .slice(0, 3)
      .map(([key, item]) => `${key}: ${String(item)}`);
    const text = parts.join(" · ");
    return text.length > 180 ? `${text.slice(0, 177)}…` : text;
  }
  return String(value);
}

function columnKeys(rows: Row[]): string[] {
  const present = new Set<string>();
  for (const row of rows.slice(0, 30)) {
    for (const key of Object.keys(row)) present.add(key);
  }
  const preferred = PREFERRED_COLUMNS.filter((key) => present.has(key));
  const rest = [...present].filter((key) => !preferred.includes(key));
  return [...preferred, ...rest].slice(0, COLUMN_LIMIT);
}

function haystack(row: Row): string {
  const parts: string[] = [];
  let size = 0;
  const push = (text: string) => {
    if (size > 4000) return;
    parts.push(text);
    size += text.length;
  };
  const visit = (value: unknown, depth: number) => {
    if (size > 4000 || value == null || depth > 2) return;
    if (typeof value !== "object") {
      push(String(value));
      return;
    }
    if (Array.isArray(value)) {
      for (const item of value.slice(0, 40)) visit(item, depth + 1);
      return;
    }
    for (const item of Object.values(value as Row)) visit(item, depth + 1);
  };
  visit(row, 0);
  return parts.join("\u0000").toLowerCase();
}

function pretty(value: unknown): string {
  const text = JSON.stringify(value, null, 2) ?? "";
  if (text.length > 80_000) return `${text.slice(0, 80_000)}\n… truncated`;
  return text;
}

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

function StateChart({ data }: { data: { state: string; count: number }[] }) {
  if (data.length === 0) return <p className="muted">No virtual machines yet.</p>;
  const max = Math.max(...data.map((item) => item.count), 1);
  return (
    <div className="bars" role="img" aria-label="Virtual machines by state">
      {data.map((item) => (
        <div className="bar-row" key={item.state}>
          <span>{item.state}</span>
          <div className="bar-track">
            <div className="bar-fill" style={{ width: `${(item.count / max) * 100}%` }} />
          </div>
          <strong>{item.count}</strong>
        </div>
      ))}
    </div>
  );
}

export function Overview() {
  const health = useQuery({
    queryKey: ["rows", "health"],
    queryFn: () => api<{ rows: Row[] }>("/api/v1/resources/health"),
  });
  const compliance = useQuery({
    queryKey: ["rows", "compliance"],
    queryFn: () => api<{ rows: Row[] }>("/api/v1/resources/compliance"),
  });
  const vms = useQuery({
    queryKey: ["rows", "vms", ""],
    queryFn: () => api<{ rows: Row[] }>("/api/v1/resources/vms"),
  });
  const row = health.data?.rows?.[0] ?? {};
  const report = compliance.data?.rows?.[0] ?? {};
  const chart = useMemo(() => {
    const counts = new Map<string, number>();
    for (const vm of vms.data?.rows ?? []) {
      const state = cell(vm.state) || "unknown";
      counts.set(state, (counts.get(state) ?? 0) + 1);
    }
    return [...counts.entries()].map(([state, count]) => ({ state, count }));
  }, [vms.data]);
  const error = health.error ?? compliance.error ?? vms.error;
  const loading = !error && (health.isLoading || compliance.isLoading || vms.isLoading);

  return (
    <>
      <header className="top">
        <h1>Overview</h1>
      </header>
      {error ? <p className="error">{(error as Error).message}</p> : null}
      {loading ? <p className="muted">Loading cluster…</p> : null}
      <div className="grid">
        <div className="card">
          <span className="muted">Cluster</span>
          <strong className="metric">{cell(row.headline) || cell(row.status) || "—"}</strong>
        </div>
        <div className="card">
          <span className="muted">Ready nodes</span>
          <strong className="metric">{cell(row.ready_nodes) || "—"}</strong>
        </div>
        <div className="card">
          <span className="muted">Virtual machines</span>
          <strong className="metric">{cell(row.vm_count) || String(vms.data?.rows.length ?? "—")}</strong>
        </div>
        <div className="card">
          <span className="muted">Running VMs</span>
          <strong className="metric">{cell(report.running_vms) || "—"}</strong>
        </div>
      </div>
      <div className="card chart-card">
        <span className="muted">VMs by state</span>
        <StateChart data={chart} />
      </div>
    </>
  );
}

function ListField({
  field,
  value,
  onChange,
}: {
  field: Field;
  value: string | boolean | undefined;
  onChange: (value: string) => void;
}) {
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
                    const next = rows.map((item, itemIndex) =>
                      itemIndex === index ? { ...item, [column.name]: event.target.checked } : item,
                    );
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
                  const next = rows.map((item, itemIndex) =>
                    itemIndex === index ? { ...item, [column.name]: event.target.value } : item,
                  );
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

function Fields({
  fields,
  values,
  onChange,
}: {
  fields: Field[];
  values: Record<string, string | boolean>;
  onChange: (name: string, value: string | boolean) => void;
}) {
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
            ) : field.name.includes("nix") ||
              field.name.endsWith("_pem") ||
              field.name === "public_key" ||
              field.name === "cloud_init_user_data" ||
              field.name === "sqlite_base64" ? (
              <textarea value={String(values[field.name] ?? "")} onChange={(event) => onChange(field.name, event.target.value)} />
            ) : (
              <input
                type={field.kind === "number" ? "number" : "text"}
                value={String(values[field.name] ?? "")}
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
  submit: (form: Record<string, string | boolean>) => Promise<void>;
};

function Pager({ page, total, onPage }: { page: number; total: number; onPage: (page: number) => void }) {
  if (total <= PAGE_SIZE) return null;
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const safe = Math.min(page, pages - 1);
  const start = safe * PAGE_SIZE + 1;
  const end = Math.min(total, start + PAGE_SIZE - 1);
  return (
    <div className="pager">
      <button type="button" disabled={safe === 0} onClick={() => onPage(safe - 1)}>
        Previous
      </button>
      <span className="muted">
        {start}–{end} of {total}
      </span>
      <button type="button" disabled={safe + 1 >= pages} onClick={() => onPage(safe + 1)}>
        Next
      </button>
    </div>
  );
}

function DataTable({ rows, trailing, onRow }: { rows: Row[]; trailing?: (row: Row) => ReactNode; onRow?: (row: Row) => void }) {
  const [page, setPage] = useState(0);
  const keys = useMemo(() => columnKeys(rows), [rows]);
  const pages = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const safe = Math.min(page, pages - 1);
  const slice = rows.slice(safe * PAGE_SIZE, safe * PAGE_SIZE + PAGE_SIZE);
  if (rows.length === 0) return <p className="muted">Nothing here yet.</p>;
  return (
    <>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              {keys.map((key) => (
                <th key={key}>{key}</th>
              ))}
              {trailing ? <th /> : null}
            </tr>
          </thead>
          <tbody>
            {slice.map((row, index) => (
              <tr
                key={cell(row.id) || cell(row.name) || String(safe * PAGE_SIZE + index)}
                className={onRow ? "inspect" : undefined}
                onClick={onRow ? () => onRow(row) : undefined}
              >
                {keys.map((key) => {
                  const text = cellText(row[key]);
                  return (
                    <td key={key} title={text}>
                      {text}
                    </td>
                  );
                })}
                {trailing ? <td>{trailing(row)}</td> : null}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <Pager page={safe} total={rows.length} onPage={setPage} />
    </>
  );
}

function Summary({ row }: { row: Row }) {
  const scalars = Object.entries(row).filter(([, value]) => !isObjectArray(value));
  const tables = Object.entries(row).filter((entry): entry is [string, Row[]] => isObjectArray(entry[1]));
  return (
    <>
      <div className="grid">
        {scalars.map(([key, value]) => (
          <div className="card" key={key}>
            <span className="muted">{key}</span>
            <strong>{cellText(value) || "—"}</strong>
          </div>
        ))}
      </div>
      {tables.map(([key, nested]) => (
        <section key={key} className="nested">
          <h2>{key}</h2>
          <DataTable rows={nested} />
        </section>
      ))}
    </>
  );
}

function isSummary(rows: Row[]): boolean {
  if (rows.length !== 1) return false;
  const row = rows[0];
  return Object.values(row).some(isObjectArray) || Object.keys(row).length > COLUMN_LIMIT;
}

export function ResourcePage() {
  const { id } = useParams({ from: "/r/$id" });
  const queryClient = useQueryClient();
  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api<Catalog>("/api/v1/catalog"),
  });
  const resource = catalog.data?.resources.find((item) => item.id === id);
  const [volume, setVolume] = useState("");
  const [filter, setFilter] = useState("");
  const [notice, setNotice] = useState("");
  const [request, setRequest] = useState<FormRequest | null>(null);
  const [detail, setDetail] = useState("");
  const [detailOpen, setDetailOpen] = useState(false);

  useEffect(() => {
    setVolume("");
    setFilter("");
    setNotice("");
    setRequest(null);
    setDetailOpen(false);
  }, [id]);

  const listKey = id === "snapshots" ? volume : "";
  const rows = useQuery({
    queryKey: ["rows", id, listKey],
    enabled: Boolean(resource?.list),
    queryFn: () => {
      const query = id === "snapshots" && volume ? `?volume=${encodeURIComponent(volume)}` : "";
      return api<{ rows: Row[] }>(`/api/v1/resources/${id}${query}`);
    },
  });

  const indexed = useMemo(
    () => (rows.data?.rows ?? []).map((row) => ({ row, haystack: haystack(row) })),
    [rows.data],
  );
  const visible = useMemo(() => {
    const query = filter.trim().toLowerCase();
    if (!query) return indexed.map((item) => item.row);
    return indexed.filter((item) => item.haystack.includes(query)).map((item) => item.row);
  }, [indexed, filter]);
  const summary = isSummary(visible);

  async function run(action: Action, row?: Row, form: Record<string, string | boolean> = {}) {
    if (!resource) return;
    if (action.client && row) {
      window.location.assign(`/vms/${cell(row[resource.idField])}/console`);
      return;
    }
    const idValue = row ? cell(row[resource.idField]) : cell(form.name);
    const result = await api<unknown>(`/api/v1/resources/${resource.id}/actions`, {
      method: "POST",
      body: JSON.stringify({
        action: action.name,
        id: idValue,
        confirm: action.confirm || Boolean(form.confirm),
        extra: { ...(row ?? {}), ...payload(action.fields, form) },
      }),
    });
    setNotice(pretty(result));
    await queryClient.invalidateQueries({ queryKey: ["rows", resource.id] });
  }

  async function create(form: Record<string, string | boolean>) {
    if (!resource) return;
    const result = await api<unknown>(`/api/v1/resources/${resource.id}`, {
      method: "POST",
      body: JSON.stringify(payload(resource.fields, form)),
    });
    setNotice(pretty(result));
    await queryClient.invalidateQueries({ queryKey: ["rows", resource.id] });
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
      rowKey: options.row ? cell(options.row[resource.idField]) : "",
      submit,
    });
  }

  async function openDetail(row: Row) {
    if (!resource?.inspect) return;
    const key = cell(row[resource.idField]);
    const body = await api<unknown>(`/api/v1/resources/${resource.id}/items/${encodeURIComponent(key)}`);
    setDetail(pretty(body));
    setDetailOpen(true);
  }

  if (!resource && catalog.isSuccess) {
    return <p className="error">Unknown resource.</p>;
  }

  const actions = (row?: Row) =>
    resource ? <RowActions resource={resource} row={row} onOpen={openForm} onRun={run} onInspect={openDetail} /> : null;

  return (
    <>
      <header className="top">
        <div>
          <h1>{resource?.title ?? id}</h1>
          {resource ? (
            <p className="muted">
              {resource.kctl ? `kctl ${resource.kctl}` : ""}
              {resource.terraform.length ? ` · ${resource.terraform.join(", ")}` : ""}
            </p>
          ) : null}
        </div>
      </header>
      <div className="toolbar">
        {resource?.list ? (
          <input
            placeholder="Filter"
            aria-label="Filter"
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
          />
        ) : null}
        {id === "snapshots" ? (
          <input placeholder="Volume" aria-label="Volume" value={volume} onChange={(event) => setVolume(event.target.value)} />
        ) : null}
        {resource?.creatable ? (
          <button
            type="button"
            className="primary"
            onClick={() =>
              openForm(`Create ${resource.title}`, resource.fields, create, {
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
                  onClick={() => openForm(action.label, action.fields, (form) => run(action, undefined, form), { confirm: action.confirm })}
                >
                  {action.label}
                </button>
              ),
            )
          : null}
        {summary && visible[0] ? actions(visible[0]) : null}
      </div>
      {catalog.isLoading ? <p className="muted">Loading…</p> : null}
      {rows.isError ? <p className="error">{(rows.error as Error).message}</p> : null}
      {resource && !resource.list ? <p className="muted">The controller has no list for this resource. Create it, then delete by name.</p> : null}
      {resource?.list && rows.isLoading ? <p className="muted">Loading…</p> : null}
      {resource?.list && rows.isSuccess ? (
        summary && visible[0] ? (
          <Summary row={visible[0]} />
        ) : (
          <DataTable
            key={filter}
            rows={visible}
            onRow={resource.inspect ? (row) => openDetail(row).catch((error: Error) => setNotice(error.message)) : undefined}
            trailing={(row) => actions(row)}
          />
        )
      ) : null}
      {notice ? <pre className="log">{notice}</pre> : null}
      <FormDialog
        request={request}
        onClose={() => setRequest(null)}
        onError={(message) => setNotice(message)}
      />
      <Dialog.Root open={detailOpen} onOpenChange={setDetailOpen}>
        <Dialog.Portal>
          <Dialog.Overlay className="overlay" />
          <Dialog.Content className="dialog wide">
            <Dialog.Title>Detail</Dialog.Title>
            <pre className="detail">{detail}</pre>
            <div className="buttons">
              <Dialog.Close asChild>
                <button type="button">Close</button>
              </Dialog.Close>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </>
  );
}

function RowActions({
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
  onInspect: (row: Row) => Promise<void>;
}) {
  const idValue = row ? cell(row[resource.idField]) : "";
  return (
    <div className="row-actions" onClick={(event) => event.stopPropagation()}>
      {resource.inspect && row ? (
        <button type="button" onClick={() => onInspect(row).catch((error: Error) => alert(error.message))}>
          Inspect
        </button>
      ) : null}
      {resource.actions.map((action) =>
        action.client ? (
          <a key={action.name} href={`/vms/${idValue}/console`}>
            {action.label}
          </a>
        ) : action.fields.length || action.confirm || action.preload ? (
          <button
            type="button"
            key={action.name}
            className={action.confirm ? "danger" : ""}
            onClick={() =>
              onOpen(action.label, action.fields, (form) => onRun(action, row ?? {}, form), {
                confirm: action.confirm,
                preload: action.preload,
                requireForce: action.name === "reset-migrate",
                row,
              })
            }
          >
            {action.label}
          </button>
        ) : (
          <button
            type="button"
            key={action.name}
            onClick={() => row && onRun(action, row).catch((error: Error) => alert(error.message))}
          >
            {action.label}
          </button>
        ),
      )}
    </div>
  );
}

function FormDialog({
  request,
  onClose,
  onError,
}: {
  request: FormRequest | null;
  onClose: () => void;
  onError: (message: string) => void;
}) {
  const [values, setValues] = useState<Record<string, string | boolean>>({});
  const [note, setNote] = useState("");
  const [ready, setReady] = useState(true);
  const [loadError, setLoadError] = useState("");
  const wide = request?.fields.some((field) => field.kind === "json") ?? false;
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
    api<{ spec?: Record<string, unknown> }>(
      `/api/v1/resources/${request.resourceId}/items/${encodeURIComponent(request.rowKey)}`,
    )
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
          id: cell(values.name),
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
        <Dialog.Content className={wide ? "dialog wide" : "dialog"}>
          <Dialog.Title>{request?.title ?? ""}</Dialog.Title>
          {request?.confirm ? <p>This changes the cluster.</p> : null}
          {request?.requireForce ? (
            <p>Force clears a stranded receive session on the named node. The controller refuses the node that owns the running VM.</p>
          ) : null}
          {request?.preload ? <p>Loaded from the current cluster spec. Apply sends this form back as the replacement.</p> : null}
          {loadError ? <p className="error">{loadError}</p> : null}
          <Fields
            fields={request?.fields ?? []}
            values={values}
            onChange={(name, value) => setValues((current) => ({ ...current, [name]: value }))}
          />
          {note ? <pre className="detail">{note}</pre> : null}
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
