import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";
import { createColumnHelper, flexRender, getCoreRowModel, useReactTable } from "@tanstack/react-table";
import * as Dialog from "@radix-ui/react-dialog";
import { Bar, BarChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { api, payload, specToValues, type Action, type Catalog, type Field, type Resource, type Row } from "./api";

function cell(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
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
    queryKey: ["rows", "vms"],
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

  return (
    <>
      <header className="top">
        <h1>Overview</h1>
      </header>
      {error ? <p className="error">{(error as Error).message}</p> : null}
      <div className="grid">
        <div className="card">
          <span className="muted">Cluster</span>
          <strong>{cell(row.headline) || cell(row.status) || "—"}</strong>
        </div>
        <div className="card">
          <span className="muted">Ready nodes</span>
          <strong>{cell(row.ready_nodes) || "—"}</strong>
        </div>
        <div className="card">
          <span className="muted">Virtual machines</span>
          <strong>{cell(row.vm_count) || String(vms.data?.rows.length ?? "—")}</strong>
        </div>
        <div className="card">
          <span className="muted">Running VMs</span>
          <strong>{cell(report.running_vms) || "—"}</strong>
        </div>
      </div>
      <div className="card" style={{ marginTop: 12, height: 260 }}>
        <ResponsiveContainer width="100%" height="100%">
          <BarChart data={chart}>
            <XAxis dataKey="state" stroke="#93a0b0" />
            <YAxis allowDecimals={false} stroke="#93a0b0" />
            <Tooltip />
            <Bar dataKey="count" fill="#3b82f6" />
          </BarChart>
        </ResponsiveContainer>
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
          <button
            type="button"
            onClick={() => write(rows.filter((_, itemIndex) => itemIndex !== index))}
          >
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
          <ListField
            key={field.name}
            field={field}
            value={values[field.name]}
            onChange={(value) => onChange(field.name, value)}
          />
        ) : (
          <label key={field.name}>
            {field.label}
            {field.required ? " *" : ""}
            {field.kind === "bool" ? (
              <input
                type="checkbox"
                checked={Boolean(values[field.name])}
                onChange={(event) => onChange(field.name, event.target.checked)}
              />
            ) : field.name.includes("nix") || field.name.endsWith("_pem") || field.name === "public_key" || field.name === "cloud_init_user_data" || field.name === "sqlite_base64" ? (
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
  const [open, setOpen] = useState(false);
  const [title, setTitle] = useState("");
  const [fields, setFields] = useState<Field[]>([]);
  const [values, setValues] = useState<Record<string, string | boolean>>({});
  const [submit, setSubmit] = useState<((values: Record<string, string | boolean>) => Promise<void>) | null>(null);
  const [checks, setChecks] = useState<Action[]>([]);
  const [dialogNote, setDialogNote] = useState("");
  const [detail, setDetail] = useState("");
  const [detailOpen, setDetailOpen] = useState(false);

  const rows = useQuery({
    queryKey: ["rows", id, volume],
    enabled: Boolean(resource?.list),
    queryFn: () => {
      const query = id === "snapshots" && volume ? `?volume=${encodeURIComponent(volume)}` : "";
      return api<{ rows: Row[] }>(`/api/v1/resources/${id}${query}`);
    },
  });

  const visible = (rows.data?.rows ?? []).filter((row) => {
    if (!filter) return true;
    return JSON.stringify(row).toLowerCase().includes(filter.toLowerCase());
  });

  const columns = useMemo(() => {
    const helper = createColumnHelper<Row>();
    const keys = Object.keys(visible[0] ?? {}).slice(0, 8);
    const data = keys.map((key) =>
      helper.accessor((row) => cell(row[key]), { id: key, header: key }),
    );
    const actions = helper.display({
      id: "actions",
      header: "",
      cell: (info) =>
        resource ? (
          <RowActions resource={resource} row={info.row.original} onRun={run} onInspect={openDetail} />
        ) : null,
    });
    return [...data, actions];
  }, [visible, resource]);

  const table = useReactTable({ data: visible, columns, getCoreRowModel: getCoreRowModel() });

  function openDialog(
    nextTitle: string,
    nextFields: Field[],
    runSubmit: (form: Record<string, string | boolean>) => Promise<void>,
    nextChecks: Action[] = [],
  ) {
    setTitle(nextTitle);
    setFields(nextFields);
    setValues({});
    setSubmit(() => runSubmit);
    setChecks(nextChecks);
    setDialogNote("");
    setOpen(true);
  }

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
    setNotice(JSON.stringify(result, null, 2));
    await queryClient.invalidateQueries({ queryKey: ["rows", resource.id] });
  }

  async function create(form: Record<string, string | boolean>) {
    if (!resource) return;
    const result = await api<unknown>(`/api/v1/resources/${resource.id}`, {
      method: "POST",
      body: JSON.stringify(payload(resource.fields, form)),
    });
    setNotice(JSON.stringify(result, null, 2));
    await queryClient.invalidateQueries({ queryKey: ["rows", resource.id] });
  }

  async function openDetail(row: Row) {
    if (!resource?.inspect) return;
    const key = cell(row[resource.idField]);
    const body = await api<unknown>(`/api/v1/resources/${resource.id}/items/${encodeURIComponent(key)}`);
    setDetail(JSON.stringify(body, null, 2));
    setDetailOpen(true);
  }

  async function runCheck(action: Action) {
    try {
      const result = await api<unknown>(`/api/v1/resources/${resource?.id}/actions`, {
        method: "POST",
        body: JSON.stringify({
          action: action.name,
          id: cell(values.name),
          confirm: Boolean(values.confirm),
          extra: payload(fields, values),
        }),
      });
      setDialogNote(JSON.stringify(result, null, 2));
    } catch (error) {
      setDialogNote((error as Error).message);
    }
  }

  if (!resource && catalog.isSuccess) {
    return <p className="error">Unknown resource.</p>;
  }

  const wide = fields.some((field) => field.kind === "json");

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
        <input placeholder="Filter" value={filter} onChange={(event) => setFilter(event.target.value)} />
        {id === "snapshots" ? (
          <input placeholder="Volume" value={volume} onChange={(event) => setVolume(event.target.value)} />
        ) : null}
        {resource?.creatable ? (
          <button
            className="primary"
            onClick={() =>
              openDialog(
                `Create ${resource.title}`,
                resource.fields,
                create,
                resource.actions.filter((action) => action.name === "plan" || action.name === "classify"),
              )
            }
          >
            Create
          </button>
        ) : null}
        {resource && !resource.list
          ? resource.actions.map((action) =>
              action.client ? null : (
                <button key={action.name} onClick={() => openDialog(action.label, action.fields, (form) => run(action, undefined, form))}>
                  {action.label}
                </button>
              ),
            )
          : null}
      </div>
      {rows.isError ? <p className="error">{(rows.error as Error).message}</p> : null}
      {resource && !resource.list ? <p className="muted">The controller has no list for this resource. Create it, then delete by name.</p> : null}
      <table>
        <thead>
          {table.getHeaderGroups().map((group) => (
            <tr key={group.id}>
              {group.headers.map((header) => (
                <th key={header.id}>{flexRender(header.column.columnDef.header, header.getContext())}</th>
              ))}
            </tr>
          ))}
        </thead>
        <tbody>
          {table.getRowModel().rows.map((row) => (
            <tr
              key={row.id}
              className={resource?.inspect ? "inspect" : undefined}
              onClick={() => {
                openDetail(row.original).catch((error: Error) => setNotice(error.message));
              }}
            >
              {row.getVisibleCells().map((cellItem) => (
                <td key={cellItem.id}>{flexRender(cellItem.column.columnDef.cell, cellItem.getContext())}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {notice ? <pre className="log">{notice}</pre> : null}
      <Dialog.Root open={open} onOpenChange={setOpen}>
        <Dialog.Portal>
          <Dialog.Overlay className="overlay" />
          <Dialog.Content className={wide ? "dialog wide" : "dialog"}>
            <Dialog.Title>{title}</Dialog.Title>
            <Fields fields={fields} values={values} onChange={(name, value) => setValues((current) => ({ ...current, [name]: value }))} />
            {dialogNote ? <pre className="detail">{dialogNote}</pre> : null}
            <div className="buttons">
              {checks.map((action) => (
                <button key={action.name} type="button" onClick={() => runCheck(action)}>
                  {action.label}
                </button>
              ))}
              <Dialog.Close asChild>
                <button>Cancel</button>
              </Dialog.Close>
              <button
                className="primary"
                onClick={() => {
                  setOpen(false);
                  submit?.(values).catch((error: Error) => setNotice(error.message));
                }}
              >
                Apply
              </button>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
      <Dialog.Root open={detailOpen} onOpenChange={setDetailOpen}>
        <Dialog.Portal>
          <Dialog.Overlay className="overlay" />
          <Dialog.Content className="dialog wide">
            <Dialog.Title>Detail</Dialog.Title>
            <pre className="detail">{detail}</pre>
            <div className="buttons">
              <Dialog.Close asChild>
                <button>Close</button>
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
  onRun,
  onInspect,
}: {
  resource: Resource;
  row: Row;
  onRun: (action: Action, row: Row) => Promise<void>;
  onInspect: (row: Row) => Promise<void>;
}) {
  const id = cell(row[resource.idField]);
  return (
    <div className="row-actions" onClick={(event) => event.stopPropagation()}>
      {resource.inspect ? (
        <button onClick={() => onInspect(row).catch((error: Error) => alert(error.message))}>Inspect</button>
      ) : null}
      {resource.actions.map((action) =>
        action.client ? (
          <a key={action.name} href={`/vms/${id}/console`}>
            {action.label}
          </a>
        ) : action.fields.length || action.confirm || action.preload ? (
          <ActionButton key={action.name} resource={resource} action={action} row={row} onRun={onRun} />
        ) : (
          <button key={action.name} onClick={() => onRun(action, row).catch((error: Error) => alert(error.message))}>
            {action.label}
          </button>
        ),
      )}
    </div>
  );
}

function ActionButton({
  resource,
  action,
  row,
  onRun,
}: {
  resource: Resource;
  action: Action;
  row: Row;
  onRun: (action: Action, row: Row, form?: Record<string, string | boolean>) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [values, setValues] = useState<Record<string, string | boolean>>({});
  const [loadError, setLoadError] = useState("");
  const [ready, setReady] = useState(!action.preload);
  const wide = action.fields.some((field) => field.kind === "json");
  const blocked = (action.name === "reset-migrate" && !values.force) || (action.preload && !ready);

  useEffect(() => {
    if (!open || !action.preload) return;
    let cancel = false;
    setReady(false);
    setLoadError("");
    const key = cell(row[resource.idField]);
    api<{ spec?: Record<string, unknown> }>(`/api/v1/resources/${resource.id}/items/${encodeURIComponent(key)}`)
      .then((body) => {
        if (cancel) return;
        if (!body.spec) {
          setLoadError("The controller returned no spec. Update was not loaded.");
          return;
        }
        setValues(specToValues(action.fields, body.spec));
        setReady(true);
      })
      .catch((error: Error) => {
        if (!cancel) setLoadError(error.message);
      });
    return () => {
      cancel = true;
    };
  }, [open, action, resource.id, resource.idField, row]);

  return (
    <Dialog.Root open={open} onOpenChange={setOpen}>
      <Dialog.Trigger asChild>
        <button className={action.confirm ? "danger" : ""}>{action.label}</button>
      </Dialog.Trigger>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className={wide ? "dialog wide" : "dialog"}>
          <Dialog.Title>{action.label}</Dialog.Title>
          {action.confirm ? <p>This changes the cluster.</p> : null}
          {action.name === "reset-migrate" ? (
            <p>Force clears a stranded receive session on the named node. The controller refuses the node that owns the running VM.</p>
          ) : null}
          {action.preload ? <p>Loaded from the current cluster spec. Apply sends this form back as the replacement.</p> : null}
          {loadError ? <p className="error">{loadError}</p> : null}
          <Fields fields={action.fields} values={values} onChange={(name, value) => setValues((current) => ({ ...current, [name]: value }))} />
          <div className="buttons">
            <Dialog.Close asChild>
              <button>Cancel</button>
            </Dialog.Close>
            <button
              className="primary"
              disabled={blocked}
              onClick={() => {
                if (blocked) return;
                setOpen(false);
                onRun(action, row, values).catch((error: Error) => alert(error.message));
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
