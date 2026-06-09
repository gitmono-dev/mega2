export const meta = {
  name: 'config-doc-audit',
  description: 'Verify docs/config.md claims against current monoengine source, critique across evaluation dimensions, synthesize an edit plan',
  phases: [
    { title: 'Verify facts' },
    { title: 'Critique dimensions' },
    { title: 'Synthesize' },
  ],
}

const DOC = '/run/media/eli/data/GitMono/monoengine/docs/config.md'
const ROOT = '/run/media/eli/data/GitMono/monoengine'

const FACTS_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  properties: {
    cluster: { type: 'string' },
    claims: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        properties: {
          doc_claim: { type: 'string', description: 'the specific claim or line-ref the doc makes' },
          verdict: { type: 'string', enum: ['confirmed', 'stale', 'wrong', 'partial', 'unverifiable'] },
          code_reality: { type: 'string', description: 'what the current code actually shows' },
          correct_refs: { type: 'string', description: 'correct file:line references in current code' },
        },
        required: ['doc_claim', 'verdict', 'code_reality', 'correct_refs'],
      },
    },
    missed_by_doc: { type: 'array', items: { type: 'string' }, description: 'facts present in current code that the doc does not mention but should' },
  },
  required: ['cluster', 'claims', 'missed_by_doc'],
}

const CRITIQUE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  properties: {
    dimension: { type: 'string' },
    doc_self_score: { type: 'string', description: "what the document own evaluation table claims for this dimension, if any" },
    my_assessment: { type: 'string', description: 'your grounded assessment in 3-6 sentences' },
    staleness_errors: { type: 'array', items: { type: 'string' }, description: 'concrete factual or staleness errors in the doc affecting this dimension' },
    gaps: { type: 'array', items: { type: 'string' }, description: 'missing considerations for this dimension' },
    improvements: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        properties: {
          section: { type: 'string', description: 'doc section or heading to change' },
          change: { type: 'string', description: 'concrete change to make' },
        },
        required: ['section', 'change'],
      },
    },
  },
  required: ['dimension', 'doc_self_score', 'my_assessment', 'staleness_errors', 'gaps', 'improvements'],
}

const ctx = `You are auditing the design document ${DOC} against the ACTUAL current source tree at ${ROOT}.
CRITICAL CONTEXT (already established by the orchestrator, verify independently — do not just trust this):
- The doc has a "事实校准 (2026-06)" section claiming mail/MailConfig do NOT exist. That is now STALE. The codebase changed AFTER the doc was written.
- Confirmed: MailConfig now EXISTS (src/common/config.rs ~376), Config has a mail: Option<MailConfig> field with #[serde(default)] (~106), "mod mail;" is in src/main.rs:17, src/email/mod.rs is now a 13-line re-export shim, src/mail/mod.rs (~232 lines) is the real SmtpMailer/Mailer/NoopMailer impl, src/context/mod.rs:41-46 builds a post-vault _late_mailer after VaultCore::new (line 33), and a sibling docs/mail.md exists.
- config.rs is now 1149 lines (doc says ~1077). Many cited line numbers have shifted.
Read the ACTUAL files. Report current line numbers precisely. Your output is consumed by an automated editor — be exact, terse, factual. Use the StructuredOutput tool.`

phase('Verify facts')

const verifiers = [
  {
    label: 'verify:config.rs-model',
    cluster: 'config.rs model + Config::new + placeholder substitution',
    task: `Open ${ROOT}/src/common/config.rs. Verify these doc claims and report current line numbers:
1. pub struct Config fields and line (doc lists 14 fields at :81, OMITS mail). Confirm current field list incl. mail.
2. mail/MailConfig "不存在/未定义" claim (STALE — confirm now exists; give exact lines of MailConfig struct, Default impl, and the Config.mail field).
3. oauth/OAuthConfig: still absent? (doc says no OAuthConfig field). Confirm.
4. Sub-config struct line numbers doc cites: LogConfig:254, DbConfig:277, MonoConfig:304, PackConfig:392, LFSConfig:549, BlameConfig:627, BuildConfig:664, RedisConfig:424, BuckConfig:735 (+ validate:831), ObjectStorageConfig:612, OrionServerConfig:673, SidebarConfig:893, ArtifactGcConfig:332. Report which are now wrong and the correct lines.
5. Config::new is synchronous at :106 — confirm + current line; confirm the with_list_parse_key set it registers (oauth.allowed_cors_origins, monorepo.admin, monorepo.root_dirs).
6. variable_placeholder_substitute at :187 (body ~189-232) with ~9 .unwrap(). Report current line span and ACTUAL count of unwrap/expect/panic in that function.
7. DbConfig::default() db_url = postgres://mega:mega@... at :293. Confirm value + current line.
8. mock()/load_str()/load_sources() at :125/:144/:158 and whether their list_parse_key sets differ from main Config::new (doc claims load_str/load_sources weaker). Report exact current lines and the actual list keys each registers.
9. Total line count of config.rs.`,
  },
  {
    label: 'verify:loader+template',
    cluster: 'loader.rs + template.rs (file location, default gen, profile, mega_base panics)',
    task: `Open ${ROOT}/src/common/config/loader.rs and ${ROOT}/src/common/config/template.rs (and grep src for mega_base/mega_cache definitions).
Verify:
1. Config file location priority order (doc: --config, MEGA_CONFIG, cwd config/config.toml, mega_base()/etc/config.toml, autogen). Report the ACTUAL order/logic and the ConfigLoader/ConfigInput API (cli.rs uses ConfigInput{cli_path, env_path} + ConfigLoader::new(input).load()). Note doc currently describes a 5-step priority — is that accurate to loader.rs?
2. Does default-config generation render base_dir into the file? Where?
3. Profile / config.<profile>.toml logic — confirm ABSENT in loader.rs.
4. mega_base()/mega_cache() — where defined, do they panic (expect/unwrap)? Give file:line.`,
  },
  {
    label: 'verify:vault',
    cluster: 'vault_core.rs (core_key.json, delete_all, root token leak, path mapping)',
    task: `Open ${ROOT}/src/vault/integration/vault_core.rs.
Verify each doc claim with current line numbers:
1. core_key.json stores secret_shares + root_token as plaintext JSON at ~:88-99. Confirm + lines.
2. Missing core_key -> println!("Vault core key file does not exist...") + vault_storage.delete_all().expect(...) + rvault.init().expect(...) + println!("...root token: {}") at ~:70-77. Confirm exact behavior + lines.
3. Unseal uses assert!(unseal.is_ok(), ...). Confirm + line.
4. log::debug! (or tracing) records root token anywhere. Confirm + line.
5. read_secret/write_secret path mapping: do they prepend secret/{name}? Confirm the path scheme used (doc SecretRef mapping depends on this). Give the function signatures + lines.
6. Whether VaultCore::new returns Result or panics (doc wants it to return Result). Current signature.
Also skim ${ROOT}/src/server/ssh_server.rs: ssh_server_key read at :78 (read_secret(...).unwrap()?) and write at :99. Confirm current lines + whether unwrap is present.`,
  },
  {
    label: 'verify:storage',
    cluster: 'storage/mod.rs (Storage::new order, buck panic, Weak<Config>, config() expect)',
    task: `Open ${ROOT}/src/jupiter/storage/mod.rs.
Verify with current line numbers:
1. Storage::new signature async fn new(config: Arc<Config>) at :188. Confirm + line.
2. database_connection(&config.database) at :189. Confirm + line.
3. ObjectStorageFactory::build(&config.object_storage) at :203 (vault-prior object storage construction). Confirm + line.
4. buck_config.validate() panic! at :250-260 (doc inconsistently cites :253, :259, :253-260). Report the ACTUAL line of validate() call and the panic!.
5. Storage holds config as Weak<Config>; config() at :334 does upgrade().expect("Config has been dropped"). Confirm + current lines.
Report any of these that have shifted.`,
  },
  {
    label: 'verify:context+cli',
    cluster: 'context/mod.rs + cli.rs + main.rs + commands (startup order, CLI model, config cmd absence)',
    task: `Open ${ROOT}/src/context/mod.rs, ${ROOT}/src/cli.rs, ${ROOT}/src/main.rs, ${ROOT}/src/commands/mod.rs.
Verify:
1. AppContext::new startup order: Storage::new (line?), init_connection(&config.redis) (line?), VaultCore::new (line?). Doc claims 27-29 / 30 / 33. Confirm current lines AND note that a post-vault _late_mailer SmtpMailer is built at ~41-46 and init_monorepo runs AFTER vault (~48) — doc does NOT mention these.
2. cli.rs: Config::new still called unconditionally BEFORE matches.subcommand() dispatch? Doc says cli.rs:37 and "先强制 Config::new 再 dispatch". Confirm current lines (ConfigLoader::new(input).load() then Config::new then init_log then subcommand match).
3. builtin_exec signature is fn(Config, &ArgMatches) -> MegaResult and builtin() returns only service + chat-migrate (NO config command). Confirm.
4. Confirm the config command family (init/validate/secret) is ABSENT.`,
  },
  {
    label: 'verify:config.toml+crossdoc',
    cluster: 'config/config.toml + docs/mail.md cross-consistency + hardcoded creds',
    task: `Open ${ROOT}/config/config.toml and ${ROOT}/docs/mail.md.
Verify:
1. Is there a [mail] section in config.toml? What fields/shape? Given MailConfig now exists with #[serde(default)] + Option, is the [mail] section now CONSUMED (not "silently ignored" as doc claims)? Report the [mail] field shape vs MailConfig fields (enabled/smtp_host/smtp_port/username/password/from/starttls).
2. Is there an [oauth] section? Confirm it is still NOT consumed (no OAuthConfig). Report its fields.
3. Hardcoded postgres creds: config.toml db_url value + line (doc says postgres://mono:mono@... at :28). DbConfig::default uses mega:mega. Report actual config.toml value.
4. Any orion postgres://postgres:postgres@... present? Where?
5. docs/mail.md: summarize what it claims about mail module status and SecretRef/password migration, so config.md can be made CONSISTENT with it. Flag any contradictions between the two docs.`,
  },
]

const facts = (await parallel(verifiers.map(v => () =>
  agent(`${ctx}\n\nCLUSTER: ${v.cluster}\n\n${v.task}`, { label: v.label, phase: 'Verify facts', schema: FACTS_SCHEMA })
))).filter(Boolean)

const factsJson = JSON.stringify(facts, null, 1)
log(`Verified ${facts.length} clusters; ${facts.reduce((n, f) => n + (f.claims ? f.claims.length : 0), 0)} claims checked`)

phase('Critique dimensions')

const dims = [
  ['合理性 (Reasonableness)', 'Is the core thesis still sound given mail now exists? Is the bootstrap-cycle reasoning (Config to Storage(DB) to Vault) correct? Is the 4-class field taxonomy still apt now that a real post-vault consumer (mail) exists?'],
  ['可行性 (Feasibility)', 'Phasing realism. Note: phase-5 prerequisite work (define MailConfig, wire mod email/mail, post-vault SmtpMailer) appears ALREADY DONE. Does the phase plan still make sense, or must phases be re-baselined? Effort estimates still valid?'],
  ['完整性 (Completeness)', 'What is missing now: src/mail as a real module, email shim, docs/mail.md cross-ref, email_jobs outbox/EmailDispatcher/notification, late_mailer being unused (_) demonstration only, config.rs growth, the actual current [mail] section being consumed. Does the doc cover the full picture?'],
  ['安全性 (Security)', 'Vault core_key.json plaintext, root token leakage (println/assert/debug), delete_all on missing, fail-closed, SecretString, stdin writes, redaction. Is the security analysis still accurate and adequately scoped? Now that mail.password is a REAL plaintext Option<String> field actually used by SmtpMailer::new (credentials), does the doc correctly state the present exposure?'],
  ['功能正确性与接口兼容性 (Functional correctness & interface compatibility)', 'SecretRef to read_secret(name) path mapping vs actual vault API. config module name vs config crate collision. re-export shim removal timing. Are the interface claims correct? Does the doc correctly describe MailConfig deserialize shape and the password field?'],
  ['数据流与控制流正确性 (Data/control flow correctness)', 'Verify the documented startup order matches context/mod.rs exactly (now incl. late_mailer + init_monorepo-after-vault). Is secret-resolution-after-vault and minimal-bootstrap-for-secret-commands still correct?'],
  ['性能与效率 (Performance & efficiency)', 'placeholder substitution cost, Arc sharing, resolver cache/TTL, hot-reload connection reuse. Any new perf consideration from mail (SMTP transport build, async send, dispatcher/outbox polling)?'],
  ['可靠性与容错性 (Reliability & fault tolerance)', 'Enumerate the panic/expect/unwrap/assert points the doc lists and verify each still exists at corrected lines; note any new ones (e.g. AppContext::new .expect on storage/monorepo, SmtpMailer::new error path, config() expect). Is fail-closed/rollback coverage adequate?'],
  ['兼容性与互操作性 (Compatibility & interoperability)', 'serde Option/default discipline, deprecated-field WARN, profile deep-merge array semantics, unknown-section warnings, cross-platform 0600/0700 + systemd/k8s. Does MailConfig follow the documents own compatibility rules (Option/default)? Is the [mail] section now silently consumed creating a backward-compat surprise?'],
  ['可扩展性与可维护性 (Scalability & maintainability)', 'src/config/ split plan, module responsibilities, shim strategy. Now that mail is a top-level module parallel to the planned config module, does the doc reflect this precedent? Is the split plan still coherent?'],
  ['合规性与标准符合性 (Compliance & standards)', 'secrecy/zeroize, stdin to avoid shell history, CI config matrix, field classification table, credential management best practices. Still adequate? Any over-claim?'],
]

const critiques = (await parallel(dims.map(([dim, focus]) => () =>
  agent(`${ctx}

You have the VERIFIED FACTS from phase 1 (JSON). Use them as ground truth; you may open files to confirm specifics.

VERIFIED FACTS:
${factsJson}

Now critique ${DOC} ONLY on this dimension: ${dim}
Focus: ${focus}

Read the relevant doc sections (the doc is ~835 lines; the self-evaluation table is near the end under "改进方案多维评估小结"). Report: the doc own self-score for this dimension, your grounded assessment, concrete staleness/factual errors affecting this dimension, gaps, and specific section-keyed improvements. Use StructuredOutput.`,
    { label: `critique:${dim.split(' ')[0]}`, phase: 'Critique dimensions', schema: CRITIQUE_SCHEMA }
  )
))).filter(Boolean)

log(`Collected ${critiques.length} dimensional critiques`)

phase('Synthesize')

const plan = await agent(`${ctx}

You are synthesizing an EDIT PLAN for rewriting ${DOC}. You have (A) verified facts and (B) per-dimension critiques. Produce a precise, prioritized plan the editor will execute with find/replace edits on the markdown.

VERIFIED FACTS (ground truth):
${factsJson}

DIMENSIONAL CRITIQUES:
${JSON.stringify(critiques, null, 1)}

Produce a markdown edit plan with these parts:
1. **Headline correction** — one paragraph: the single most important update (mail/MailConfig/email-shim/late_mailer now exist; phase-5 prerequisites largely landed; doc 事实校准 + 速览表 + 阶段5 + 字段分类表 + multiple sections are stale).
2. **Section-by-section edits** — an ordered list. For each: the doc section heading, what is wrong/stale/missing, and the EXACT corrected facts to write (with correct current line numbers from the facts). Cover at minimum: the 事实校准 block (points 1-3 now false), 当前实现状态速览表 rows (mail, oauth, the new src/mail module), 强类型配置结构 (add mail field), 主要消费场景 + 依赖顺序 (mail now real + late_mailer + init_monorepo-after-vault), 敏感配置四分类 + 字段分类表 (mail.password now a real migratable candidate with present plaintext exposure), SecretRef过渡策略 (password/password_ref now applicable), config init [mail] example (now generatable), 阶段5 (re-baseline: prerequisites done, remaining = SecretRef infra + password_ref migration + dispatcher wiring), 多维评估小结 table (update scores/notes), 小结 + 硬约束 + 检查清单.
3. **New content to add** — items the doc omits: src/mail first-class module + docs/mail.md cross-ref, email shim, email_jobs/EmailDispatcher/notification outbox, late_mailer is an unused demonstration, cross-platform caveats already partly present, config.rs line-count.
4. **Line-number correction table** — every stale file:line the doc cites and its corrected value, from the facts.
5. **Things to NOT change** — claims that are still accurate (CLI two-phase still needed, config command family still absent, vault core_key risks, bootstrap cycle, Storage/Redis early deps, hardcoded db creds) so the editor preserves them.
6. **Consistency guardrails** — keep config.md consistent with docs/mail.md; preserve the doc honest "limited security benefit until hardening" framing; do not over-correct into claiming mail SecretRef is done.

Be specific and exact. This plan is the editor spec.`,
    { label: 'synthesize:edit-plan', phase: 'Synthesize' })

return { facts, critiques, plan }
