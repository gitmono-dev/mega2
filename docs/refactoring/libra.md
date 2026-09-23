# Libra 協作：Agent 變更證據與可信交付重構需求

本文定義 mega2 為配合 Libra，將 Agent 的意圖、可觀測執行紀錄、候選修改與驗證結果接入 CL／主幹交付所需的重構。

> **狀態**：重構需求，尚未實作；文件 review 的 PASS 僅代表需求可作為後續設計與實作的輸入，不代表功能、效能或安全驗收通過。
> **治理**：遵循 [general.md](general.md) 的文件結構；本次交付依使用者要求僅編寫及 review 需求，不改程式碼、不 bump 版本、不提交或部署。
> **強依賴**：與 [trunk-push.md](trunk-push.md) 共用根樹寫入序列化及 landing provenance；測試場景在 [integration.md](integration.md) 登記。公開分支、Tag 與 ImportRepo 例外見 [使用指南](../user-guide.zh.md)。

## 1. 目標、範圍與非目標

目標是讓團隊能回答「誰因何需求修改了什麼、在哪個精確版本上得到哪些驗證、由誰批准、最後進入哪個主幹版本」，並把有效歷史回饋給後續 Agent 任務。產品最小閉環是跨服務修改的證據審查與合併把關。

mega2 的責任是中央接收、身分與授權、持久化、關係索引、證據有效性、批准及 landing；Libra 的責任是本機工作區、Agent adapter、捕獲、脫敏、checkpoint 與上傳。website frontend 消費 mega2 API 呈現 diff、證據及批准；本文只定義它所需的後端契約。

下列內容不在本次重構的核心交付範圍：

- 取得模型未公開的內部 chain-of-thought，或保證摘要忠實呈現全部內部推理。
- 自建通用 coding agent、聊天產品、模型路由平台、IDE 或完整 CI runner／建置系統。
- 強迫客戶遷移所有 Git 託管，或要求普通 Git 客戶端理解 Libra 物件。
- 跨独立倉庫的原子提交、資料庫／外部 API 副作用的自動回退、模型執行的位元級重現。
- 把 transcript 數量、token 數或個人 AI 使用排行作為產品成功標準。

所有本文新增的欄位、狀態、API 能力與表名稱均為**目標契約**；正式 URL、DDL、索引與错误碼在階段 0 的契約稿凍結，不能從本文示意直接宣稱已有端點。

## 2. 事實校準（2026-09-06）

依據當前工作區靜態源碼：mega2 `Cargo.toml:3` 為 `0.5.3`，Libra `Cargo.toml:3` 為 `0.22.15`；未執行整合或效能測試。程式碼錨點以路徑及符號為主，行號只描述本次快照。

| 能力／組件 | 實現狀態 | 事實、錨點與限制 |
|---|---|---|
| Repo artifacts HTTP 面 | 已實現 | [`artifacts_router.rs:35`](../../src/api/router/artifacts_router.rs)：discovery、batch、commit、sets、讀寫物件。**目前沒有 repo 授權保護**：`src/contract/policy/guard/cedar_guard.rs:52` 的 `resolve_cl_action` 只映射 `/cl`；artifacts 落入 `UnprotectedRequest` 並在 `:374` 直接放行，handler 無獨立授權；不是可直接承載可信證據的安全入口 |
| AI artifact 類型 | 已實現 | [`artifacts/mod.rs:15`](../../src/contract/api/artifacts/mod.rs)：Intent、Run、Evidence、Decision、Provenance 等枚舉；UUID OID 與 Git hash 不同 |
| 證據不可替換保證 | 部分完成 | [`artifact_service.rs:677`](../../src/jupiter/service/artifact_service.rs) 的 `commit_artifacts` 拒絕同 set ID 的不同 manifest；但 `upload_artifact_object_bytes`（`:233`）對既有同大小 UUID 仍呼叫 `put_stream`，不是內容不可變保證 |
| CL／佇列 | 已實現 | [`mega_cl.rs:10`](../../src/callisto/mega_cl.rs)、目前的 [`push_queue_service.rs`](../../src/jupiter/service/push_queue_service.rs)；既有佇列本身不能證明本提案所需的跨進程根樹序列化保證 |
| 按任務隔離 CL | 未實現 | [`monorepo.rs:1250`](../../src/ceres/pack/monorepo.rs) 的 `fetch_or_new_cl_link` 仍以 path＋username 查找 open CL |
| CL 至 landing 的完整追溯 | 部分完成 | [`mono_api_service.rs:2582`](../../src/ceres/api_service/mono_api_service.rs) 的 `merge_cl_unchecked` 合成更新；`mega_cl` 無直接的 `merge_commit_id` 欄位，尚缺本文要求的完整關係契約 |
| Bot 與授權 | 部分完成 | [`bot_router.rs:80`](../../src/api/router/bot_router.rs) 有 token 管理，`src/api/oauth/mod.rs` 有 `BotIdentity`；[`mega.cedarschema:1`](../../src/contract/policy/mega.cedarschema) 的 action principal 仍為 User。`src/contract/policy/enforcement.rs:71` 的 off／shadow 均放行，`config/config.toml:343` 的現行預設為 off；授權執行前置與完整 Agent 委託模型均需補齊 |
| Libra 捕獲／模型 | 已實現 | [Agent 命令](../../../libra/docs/commands/agent.md)、[物件模型](../../../libra/docs/ai/object-model.md)：session、checkpoint、coverage、脫敏與 snapshot/event/projection 分層；外部捕獲和 Libra 原生工作流的結構化程度不同 |
| 多來源中央匯聚 | 未實現 | [`Libra agent/push.rs:1`](../../../libra/src/command/agent/push.rs) 使用固定 `refs/libra/traces`，有 force-with-lease；不是多人中央事件攝取契約 |
| 大型工作區／依賴自動推導 | 部分完成 | Libra [sparse-view](../../../libra/docs/commands/sparse-view.md) 只過濾顯示；[deps](../../../libra/docs/commands/deps.md) 為宣告式邊，不能當成完整 sandbox、按需 materialization 或自動依賴分析 |

校正既有分析時需注意：

1. mega2 已有 AI artifacts 與 bot 基礎，不能將其寫成全部從零建置。
2. `update_branch` 的空 diff 分支已將 `(from,to)` 一併移到 target head（`mono_api_service.rs:3353`）；不得沿用舊 [delta.md](../gap/delta.md) 的 no-op 回退推論作為尚未修復的現狀。
3. 根寫入的 FIFO、鎖、CAS 與 storage-only trunk 行為在 `trunk-push.md` 是需求，不是已交付能力。本文不以其已實現為前提。
4. 兩邊 `Cargo.lock` 的 `git-internal` 分別是 `0.8.7`／`0.8.6`；不同版本不直接證明不相容，仍需 wire fixtures 驗證。
5. artifacts 源碼註解引用 `docs/artifacts-protocol.md`，目前該檔不存在；階段 0 必須建立正式契約文件或修正引用，不能以缺失文件作為現行規格依據。

## 3. 硬約束與不可違反的原則

1. **事實與解釋分離**：觀測事件、Agent 明示理由、事後生成摘要必須標記來源；缺少資料不得補造為事實，因為合併判定需要可核對證據。
2. **身分不由 payload 自證**：author、模型名、簽名與 human delegator 不等同；伺服器驗證授權後綁定 principal，避免冒名及自行提權。
3. **歷史追加，投影可重建**：修訂、否決、失效、撤銷與刪除留存指標均有事件；正常編輯不覆寫歷史。敏感內容依刪除政策移除，見 REQ-LB-09，不能用不可變性阻止合法資料治理。
4. **內容 digest 與邏輯 ID 分離**：UUID 表示身分，digest 表示位元組完整性；簽章／伺服器收據只证明來源與接收，不能证明測試或主張正確。
5. **程式碼、證據、批准精確綁定**：不得將舊 patch 的成功檢查套用到新 patch；不能只用 CL link、時間戳或可變分支名做關聯。
6. **共用根寫入者邊界**：整合 `trunk-push.md` 的單一根寫入服務，不另建並行寫 main 的 Agent 佇列；否則證據檢查與寫入間會出現競態。
7. **普通 Git 與 ImportRepo 行為不暗改**：不新增公開 heads，不開放客户端 tag，不把固定 traces ref 默認套進 monorepo 的 CL 寫入路徑；擴展必須 discovery 協商。
8. **選擇性啟用，保護模式不降級**：未啟用整合的倉庫保持既有 Git 行為；不承諾保留未授權讀寫 evidence 的舊 artifact 旁路。新 evidence 服務即使採 observe 也必須執行授權；一旦 gate 啟用 required，缺證據、依賴服務失敗或不相容客戶端均不能被當成 pass。
9. **最小權限貫穿派生資料**：列表、摘要、搜尋、下載、通知、索引與快取都適用；程式碼可讀不自動授予原始 transcript 可讀。
10. **復現承諾分層**：可恢复快照、可檢視軌跡、可重跑固定測試分別聲明；外部工具紀錄是資料，不得在查詢／回放時自動執行。

## 4. 現狀與目標對比

| 維度 | 當前狀態 | 目標狀態 | 實現難度 |
|---|---|---|---|
| 工作單元 | path＋username 隱式選 CL | 穩定 change ID，明確 task／run／CL revision | 複雜 |
| 中央同步 | 通用 UUID artifacts；Libra traces ref | 可協商、冪等、可恢復的證據攝取 | 複雜 |
| 證據可信度 | 有类型及 metadata | 來源分級、內容驗證、不可覆寫、完整性診斷 | 複雜 |
| 合併把關 | 既有 CL checks／review／queue | 對精確候選樹與政策的證據評估，寫入時重查 | 複雜 |
| 歷史追溯 | commit／CL／artifact 分散 | 原始 patch、rebase、批准、landing 的持久關係 | 複雜 |
| 下次任務上下文 | 本機紀錄為主 | 按權限、版本與有效性返回可引用歷史 | 中等 |
| 市場採用 | 自有託管後端為主 | 原生 monorepo 模式＋可選外部 SCM 證據服務 | 複雜 |

## 5. 領域契約與資料所有權

以下為邏輯模型，不要求每一行都新增一張表；實作應復用 `callisto`／`jupiter`，避免複製 Libra runtime DB。

| 邏輯實體 | 穩定識別及必要資料 | mega2 責任 |
|---|---|---|
| RepositoryIdentity | 伺服器發行的 repo ID、deployment／tenant scope、外部 provider repo ID（可選） | remote URL 只是可變別名，不作授權或租戶主鍵 |
| Change | repo ID＋change ID、owner principal（含型別與穩定 ID）、created-by principal、可選 delegator／grant、可選 intent／task ID | 一個工程變更；可有多個 run、多個 CL revision；owner 預設為經認證的建立主體，委託者不自動成為 owner |
| CLRevision | change ID、既有 CL link、revision ID、base／tip／tree IDs、root baseline、path mapping | CL 本體新增 typed owner 與不可變的 selector mode（legacy／explicit）；revision 保留 actor／delegator 引用，不覆寫舊修訂；更新用 expected revision CAS |
| Run／CaptureSource | producer instance、provider session、run ID、collector／adapter 版本、capture coverage | 外部不完整捕獲可只有 session，不强制生成虛假 Intent／Plan |
| EvidenceBundle | bundle ID、schema version、物件清單／digest、run／patch revision、脫敏版本與完整性狀態 | 管理接收、驗證、提交與冪等重送 |
| Verification | subject fingerprint、check ID、環境／工具鏈、結果、runner 身分、起止時間 | 分辨 Agent 自述、collector 觀测、受信任 runner 結果 |
| Approval | reviewer principal、subject fingerprint、policy revision、決策與時間 | 以事件追加批准、否決與撤銷；不信任 payload 自報批准者 |
| LandingRecord | change／CL revision、原始 OID 集、候選與落地樹、path／root commits、queue operation ID | 與實際根 ref 推進一致提交，支援一對多／多對一關係 |
| Projection／Outbox | 投影版本、處理水位、重送 key、錯誤狀態 | 支援可重建查詢及對外通知，不作歷史真相 |

`subject fingerprint` 至少包含：repository ID、CL／patch revision、base root commit、候選 root tree、subtree path mapping、依賴／lockfile fingerprint、測試規格、環境與工具鏈 fingerprint。policy revision 另作批准／評估的綁定鍵，不能在政策變動後繼續沿用舊評估。

只知道子樹 tip 不能证明整個 monorepo 已通過驗證。組裝候選根樹時必須驗證路徑映射、子樹來源與授權；路徑要做元件邊界正規化，拒絕 `..`、編碼繞過、前綴碰撞及跨 ImportRepo 邊界的隱式映射。

## 6. 重構需求

### REQ-LB-01 — 版本化攝取協定（P0）

**改造面**：`src/contract/api/artifacts/`、`src/api/router/artifacts_router.rs`、`src/jupiter/service/artifact_service.rs` 與其 storage/migration；必要時新增相鄰 evidence 契約域，而不是在任意 metadata 中隱藏必需欄位。

- Discovery 必須報告 supported schema、hash algorithms、limits、授權方法、上傳模式及是否支援證據驗證；不能因通用 artifact 類型存在就宣告 Libra-ready。
- 相容 v1 UUID OID；新增 evidence schema 以版本區隔，禁止把 OID 靜默改成內容 hash。未知必需版本／算法在寫入前拒絕；未知可選欄位按凍結規則處理。
- 攝取流程為 negotiate → staging upload → finalize／verify → committed receipt。只有 committed、完整且授權有效的 bundle 可以成為合併證據。
- 冪等鍵以 server scope＋repo ID＋producer ID＋bundle ID 為界；producer ID 由伺服器登記並綁定已驗證 principal，禁止 client 冒用其他 producer。查詢／重送先授權再比較；同鍵同內容返回既有收據，同鍵不同內容衝突，不以 last-write-wins 覆蓋。
- 允許事件亂序、重複與離線續傳；保存每來源序號／缺口及 server receive time，不假設全域時鐘順序。缺關聯的資料標記 incomplete，不生成假的因果關係。
- 部分缺件回傳可恢復的 missing set；未授權 OID 的存在性不得泄露。限制壓縮前後大小、物件數、單請求／單租戶配額與並發，支援背壓。

**驗收**：AC-LB-01／02／03。Libra 真客户端對接是完成條件；只有手寫 JSON 的成功測試不算端到端完成。

### REQ-LB-02 — 內容完整性與儲存發布（P0）

- digest 對明確定義的、已脫敏位元組計算，manifest 的 canonicalization、算法名稱和大小均版本化；server 自行計算／驗證，不只相信客戶端 header 或 object-store ETag。
- 所有寫入模式，包括 proxy PUT、signed PUT、multipart，先寫唯一 staging key；檢查內容後以條件建立或等效不可覆寫機制發布至 final key。已發布物件不得再發可覆寫的 signed PUT。
- final storage key 必須包含部署／租戶隔離域與 content identity，跨 repo 共享只在明確授權及 reference accounting 下啟用；對外的 UUID namespace 不構成權限。
- manifest、關聯及收據在 DB 事務內提交；物件先驗證可讀再發布 manifest。物件已寫、DB 未提交屬可回收孤兒；DB 已提交、回應遺失應可冪等查回。GC 與 finalize 需鎖／租約協作，不能刪除正在發布的內容。
- 舊 artifact 標為 legacy-unverified；授權匯入後以新 evidence manifest／final key 固定一份已驗證內容，才可用於新 gate，不直接採信仍可由 v1 修改的儲存引用。不得偽造歷史接收時間或驗證者。
- v1 相容只保證 UUID／wire 形狀，不保證匿名可讀寫 evidence。新 evidence 命名空間及內容必須在所有舊 batch／PUT／GET／manifest 介面被授權隔離或拒絕，亦不得藉相同 UUID、舊 signed URL、legacy GC 觸及 final evidence；舊端點不能成為旁路。若部署共用可達的底層 key，先隔離／迁移並驗證舊 URL 失效，再允許啟用證據服務。

**驗收**：AC-LB-02／03／12。對同 UUID、同大小、不同內容及併發 PUT 逐一驗證，不能只測 size mismatch。

### REQ-LB-03 — 任務識別與 CL 修訂（P0）

- 增加顯式建立／綁定 change 的服務契約；每個 change 的 CL 修訂可由多個 run 貢獻，不把 agent session ID 等同 change ID。
- 新客户端必須顯式指定已授權的 change／CL target 與 expected revision；協定 carrier 在階段 0 決定並由 discovery 宣告，不能假定現有 Git server 已接受 push-option。
- CL owner 是 typed principal，created-by 與 delegator 分別保存。Agent／service principal 建立的 CL 預設 owner 為該服務主體、selector mode 為 explicit；不得把 delegator 的 username 填進 owner 以冒充人。人工以新 API 建立的任務也為 explicit。轉移 owner 需明確授權及審計，不改 selector mode。
- 普通 Git 的隱式候選集合**只含 legacy mode、同 repo/path、同已解析人類 principal 的 open CL**，不包含 explicit Agent 或新任務 CL。零項按原流程建立 legacy CL，一項更新，多項拒絕歧義並給出選定目標的方法；必須替換目前 `.one()` 的不確定查找，不能任選最新 CL。明確指定 CL 的更新另驗證 target 權限。
- 同使用者同路徑新增 Agent 任務不得污染 legacy 候選集合。既有匿名／無法解析身分列的相容映射見 §8，不能將字串相同當成經驗證的人類身分。
- 同路徑多任務可同時存在。`trunk-push.md` ADR-TP-10 的每路徑一項在隊**僅約束 `kind='push'`**；CL 落地為 merge 行，由隊列全序與鎖內重查序列化，本文不新增亦不放寬 merge 的入隊約束。
- rebase、squash、人工修改及撤銷新增修訂與 lineage 邊；被淘汰修訂在保留政策內可讀。明確表示一個 run 產出多個 patch、一個 patch 混合多人／Agent 的情況。

**驗收**：AC-LB-04／05。需覆蓋 Git HTTP／SSH 與 Web 編輯三個入口的一致性。

### REQ-LB-04 — 委託身分與證據授權（P0）

- 在既有 BotIdentity／token 基礎上建立 service principal 與 delegator 關係，Cedar schema／entity builder／HTTP／Git 解析與審計同步更新，不是只新增 `Bot` 名稱。
- **啟用前置**：新增 evidence 服務預設關閉；啟用時必須同時滿足 `cedar.enforcement=enforce`、證據端點完整 action／resource 登記、身分與 policy store 可用、v1 旁路已隔離。啟動及 reload 均校驗；off／shadow 與啟用 evidence 互斥，拒絕不安全組合，不能先接收再期待 gate 擋住。gate 的 off／observe／required 與 Cedar enforcement 是兩組不同設定。
- evidence 的 negotiate、upload、finalize、查詢、approval 與 landing gate 必須有專用 fail-closed 授權路徑；不能落入現有 `UnprotectedRequest` 放行分支。未知 action、entity store 缺失、resolver 失敗一律拒絕。required 的前置還包括 evidence 服務已啟用且授權真正執行；授權失效時拒絕新操作並阻止 landing，不自動降級。
- 授權範圍至少有讀程式碼、寫候選修改、提交證據、讀原始 trace、讀衍生摘要、批准與執行 landing。批准身分不能由 Agent 上傳 Evidence 冒充。
- 有效權限為服務主體、委託授權、repo/path policy 及任務限制的交集；短期 token 帶 audience、expiry、grant ID，可撤銷，不嵌入 transcript。未委託的 service job 必須有明確 service grant，不能把缺 delegator 當作不限權。
- 接收、finalize、查詢與 landing 均按當前授權判定。委託過期不能以已入隊規避；歷史紀錄保留原 principal，後續操作需重新授權。
- 跨 repo transcript 以分片及分類處理；無法可靠切分時採取足夠嚴格的可見範圍。衍生摘要不能繞過來源權限，搜尋計數及快取亦適用。
- signed download 有最長 TTL 與明示撤銷窗口；要求即時撤銷的資料只能經每次授權的 server proxy，不宣稱已發出的簽名 URL 可立即回收。

**驗收**：AC-LB-06／07／13。新增 action 與快取版本失效必須做負向權限測試。

### REQ-LB-05 — 驗證／批准有效性與合併門（P0）

- 將捕獲完整性、來源信任、內容完整性和測試結果分開存儲；例如 incomplete trace 並不等於 failing test，但 required coverage 缺失必須阻止 gate。
- 結果至少區分 passed、failed、missing、stale、unverified、error；同一 fingerprint 上可有多個獨立 check，不讓最後一份 Agent 自述覆蓋受信任 runner 的失敗。
- 定義版本化 policy：`off`（新 gate 關閉，既有 checks 不變）、`observe`（只記錄新判定）、`required`（缺必需證據拒絕）；切換由授權管理者操作並追加審計。緊急例外需獨立權限、理由、範圍和有效期，不能假冒 passed。
- 明定自審批政策：需要人類批准的 required policy 預設禁止 owner、created-by、實際修改者及已記錄 delegator 充任唯一批准者；允許自審批須由授權管理者在版本化政策中顯式啟用並可查詢。既有 ACL 自提權與 reviewer 硬限制優先，不得由此設定削弱；service principal 不能提供人類批准。
- patch、依賴、測試規格、環境、批准対象或政策改變即重新評估。首版保守策略：root baseline 改變使候選整合證據 stale；局部證據重用是後續可選能力，需完整輸入閉包證明。
- 受信任 CI 提交使用 runner principal 與不可重放的 job／attempt 關聯，server 驗證實際受測 commit/tree；「某 pipeline 成功」不能直接映射為當前 patch 成功。
- required policy 必須覆蓋所有能落地的入口，包括一般 merge、內部 merge、queue processor。直接寫 ref、Web 寫入與 ImportRepo attach 是否影響受保護範圍，需依根寫入清單接同一 gate 或明確拒絕，不能留下旁路。

**驗收**：AC-LB-05／08／09。舊客户端在 off 下不受新 gate 阻擋；required 下回傳可操作的 missing／unsupported 診斷。

### REQ-LB-06 — 原子 landing 與主幹追溯（P0）

- 復用 `trunk-push.md` 階段 1–3 的根寫入 FIFO／事務锁／CAS。本文不重新定義隊列順序、CAS 失敗重試或 root writer 名單；與該文不一致時先修訂協同設計。
- 耗時測試在根寫入臨界區外執行；進入寫入輪次後重查 root baseline、CL revision、授權、policy、證據狀態。若已 stale，退出本輪並重新驗證，不能鎖住根等待 CI。
- 主幹 ref、landing mapping、被接受修訂、必要審計和 outbox 必須在同一 DB transaction 內持久化；不能先推 main 再 best-effort 寫溯源。
- LandingRecord 記錄原始 commit 集、path commit、root roll-up、candidate fingerprint、批准及驗證引用。commit trailer 可作導航但不是唯一權威；source commit 不可達後仍需保有符合政策的內容與關係。
- crash／重送按 operation ID 回傳同一結果，不重複合成 commit。no-op／淨零結果明確記錄為無新根 commit，不能臆造與 queue ID 一一對應的 landing SHA。
- 未來 storage-only trunk 模式不建立虛構 CL、人類批准或 website session；可記錄 transport principal 及 provenance，但不宣稱提供本文 required 審查保證。若要在該模式啟用 required，需額外定義服務身分／政策前置並完成獨立驗收，首版拒絕此組合。

**驗收**：AC-LB-09／10／12。含兩個 server process 的併發場景及 DB 提交後回應遺失場景。

### REQ-LB-07 — 評審與歷史查詢 API（P1）

- 可由 commit、CL revision、change、run、repo/path 查詢完整關係；回傳來源定位、coverage、信任、有效性與缺失原因。unknown 保持 unknown。
- website frontend 所需契約包括 diff 對應意圖摘要、被否決方案、測試版本、批准／撤銷及 landing；不要求 reviewer 預設讀取全部 transcript。
- 所有摘要保留來源 bundle／物件／事件位置、生成器及版本；人類批准與模型建議用不同類型表示。
- history retrieval 按 revision、依賴 fingerprint、有效／否決狀態及當前權限過濾。被否決方案可作反例，但不得標成推薦；新任務不自動執行歷史指令。
- 以 keyset cursor 分頁、指定投影版本／讀取水位；投影可重建，落後時回傳 pending／watermark，而非以空結果暗示無證據。合併門讀取權威狀態，不依賴最終一致的搜尋索引。

**驗收**：AC-LB-07／10／11。website frontend 變更需另行納入實作寫集、同栈測試與 README 所載 rebuild/reload 工作流；本次不修改前端。

### REQ-LB-08 — 影響分析與外部 SCM 模式（P2，可選擴展）

- 透過 adapter 消費 Nx／Bazel／Buck 等既有輸出的依賴／測試圖，固定圖版本、來源 commit、生成器及完整性；不將 Libra 手工 deps 圖當成完整真相。
- 依賴圖缺失、不支援語言或輸入閉包不完整時，required 模式要求保守測試集合或授權人工處置，不能把空圖當成無影響。最小閉環可使用管理者配置的固定必需測試集合。
- 外部 GitHub／GitLab 模式只提供證據與 checks；以 provider installation＋immutable repo ID＋PR/head SHA 關聯，verify webhook、deduplicate delivery、處理亂序並回查當前 head。對外寫 check 需客戶明確安裝授权。
- 外部合併是否受控，取決於 provider branch protection／required check 配置，必須標示已驗證／未驗證 enforcement；沒有權限確認時只能聲稱觀測。不得假稱 mega2 對外部 SCM 具有原子落地主權。
- 不能以先完成多 provider adapter、通用向量搜尋或全量工作區 provisioning 阻塞原生 CL 的最小閉環。

**驗收**：AC-LB-14／15；可選 adapter 未交付時 discovery 明示 unsupported。

### REQ-LB-09 — 保留、刪除、成本與恢復（P1；敏感資料前置為 P0）

- 原始 transcript、脫敏內容、衍生摘要、驗證證據與最小審計索引分別定義 retention；首版默認只收脫敏內容，raw 必須獨立 opt-in 及權限。脫敏失敗的 payload 不可發布。
- 刪除以可審計 tombstone 表示，按引用、legal hold（若配置）與保留政策刪除內容；摘要、搜尋索引、快取、導出副本和備份恢復均要納入。已刪內容不得被重新匯入／備份重播復活；需要 deletion ledger 及恢復水位。
- 歷史已 landing 的合併事實保留，內容到期顯示 expired／deleted；不回頭偽造「當時未驗證」。尚未 landing 的 required 證據若失去可驗證內容，阻止合併。
- GC 區分 staging lease、committed reference、active verification、landing retention 和 deletion tombstone；不得只以 blob 年齡判斷。授權隔離域內去重，限制大小、頻率、保留量及索引增長。
- 定義備份一致性點、物件／DB／deletion ledger restore 順序、重建驗證與恢復演練；恢復到無法证明完整性時只提供受限讀取，不啟用 required landing。
- 指標包括接受／拒絕／缺件／stale 數、上傳與索引延遲、每變更儲存量、GC／outbox backlog、追溯成功率；不得在 telemetry 輸出原始 prompt 或 secret。

**驗收**：AC-LB-03／07／11／12／13。無 raw 隔離與刪除保證時，不開放 raw 接收。

## 7. 前置依賴矩陣與決策邊界

| 本文工作 | 依賴 | 類型 | 關鍵同步點 |
|---|---|---|---|
| LB-01／02 | 現有 artifacts／orbit；[contract.md](contract.md) 邊界 | 前置 | 固定 wire schema、UUID 相容及發布原子性；不重新拆 orbit package |
| LB-03／06 | [trunk-push.md](trunk-push.md) 階段 1–3、ADR-TP-10/14/15/16 | 前置／協同 | 共用 root writer 和 provenance；ADR-TP-10 只限制 push 行，merge 不新增路徑唯一約束；no-op 無新根 commit |
| LB-04／05 | 既有 Cedar、BotIdentity；[website-auth.md](website-auth.md) 的認證分工 | 前置 | 新服務身分不另建登入面；政策快取、各入口及撤銷一致 |
| LB-01／04／09 | [config.md](config.md)、[vault.md](vault.md) 現行配置／SecretRef 約束 | 協同 | 使用既有啟動與憑據載入次序；新增 config schema／validation／reload 需同步既有規則 |
| LB-07 | website frontend API 消費 | 後置 | 後端契約先凍結；前端不讀 artifact store 或 DB 旁路授權 |
| LB-08 | 既有 CI／SCM provider adapter | 可選 | 固定測試集合即可完成首批 gate，不依赖 Orion 整體移植 |
| 全部 | [integration.md](integration.md)、[test-infra.md](test-infra.md) | 協同 | AC-LB 場景登记、migration 真實建庫、雙進程與真 Libra 客户端 |

以上 config／vault／website-auth／contract 為遵循既有約束，本文不改其設計或新增反向阻塞。新增的強協同（trunk-push、integration）已在對應文件及 README 登記。若實作需改既有硬約束，必須在該階段同步修訂受影響文件。

階段 0 必須形成可 review 的契約決策：① Git target carrier／legacy 歧義錯誤；② evidence schema／canonicalization／error codes；③ 身分及 policy entity 遷移；④ root writer 事務與 provenance schema；⑤ 上傳 backend 條件發布能力及降級拒絕規則；⑥ retention／原始資料分類；⑦ schema upgrade／rollback 邊界。本文先固定上述行為與驗收，未決的實作細節不能由單一模組暗中決定。

## 8. 遷移步驟（依賴順序，不承諾工期）

| 階段 | 具體交付 | 前置 | 可驗證出口 |
|---|---|---|---|
| 0：契約凍結 | 校準源碼與現行計畫；凍結 §7 七項決策、OpenAPI／fixtures、資料分類與錯誤語義 | 本需求 review | fixture 能表達 partial capture、mixed authors、子樹、no-op；新舊客户端矩陣無未定義格 |
| 1：可信接收 | LB-01／02、LB-04 接收讀取所需權限、LB-09 基礎隔離／保留；schema 用 additive migration | 0 | AC-01（至 bundle 查回）、02/03、06（接收／讀取）、07（既有讀取／下載面）、13（攝取／保留子集）；legacy 不能冒充 verified |
| 2：任務與 lineage | LB-03、LB-07 基本查詢；為舊 CL 建立 legacy change mapping | 1 | AC-01（含 CL 查詢）、04、05/10 的修訂及來源查詢子集；不要求尚未交付的 gate／landing 通過 |
| 3：主幹把關 | LB-05／06；復用已驗收的 root writer；先 observe 對照再逐 repo 啟用 required | 2＋trunk-push 1–3 | 完整 AC-05/06/08/09/10，AC-12 的 landing crash／migration 子集及 AC-13 的未落地 gate 子集；所有落地入口無旁路 |
| 4：團隊歷史與運維 | 完整 LB-07／09；website frontend 契約、刪除／restore／投影重建 | 3 | AC-07/10/11/12/13；版本追溯與刪除不復活，評審可看對應證據 |
| 5：可選接入 | LB-08 依賴圖與外部 SCM adapter，按獨立能力開關逐一啟用 | 4 | AC-14/15；外部 enforcement 不確定時不聲稱已保護 |

表內 AC 數字均指 `AC-LB-*`。階段 1–3 的子集驗收不能聲稱整項 AC 已完成；階段 4 出口需重跑並完整通過 AC-LB-01…13。階段 0 契約稿須將子集對應到具名測試，未交付介面標為 pending，不用軟跳過當成功。

**升級與恢復要求**：

- 先擴展 schema 再回填；既有 CL 一律設定 selector mode=legacy，保留原 username 供稽核。能從權威人類帳戶映射驗證者回填 typed human owner；已知匿名列回填 typed anonymous；無法驗證的列為 legacy-unresolved，不偽造 human/service principal。legacy-unresolved 僅在未啟用新整合的舊模式保留原查找行為；受保護 repo 啟用前必須由管理者解析歸屬或隔離這些列，resolver 不得在运行时因同名自動認領。不得從相近時間推造 run→commit 關係。逐批校驗數量、引用及 digest，回填可重入。
- off／observe／required 是政策狀態，不是軟體版本判定。只允許管理者顯式切換，記錄原因；回滾二進位不得自動將 required 降為 off。
- 新舊 server 並行必須透過能力／schema 版本 fencing，舊節點不得寫入受新 gate 保護的 repo；無法 fencing 的部署先排空寫入再升級。
- 遷移失敗不發布能力；已寫新歷史不做破壞性 down migration。故障回退優先關閉新攝取／排空 queue、保留唯讀查詢，再以前向修復恢復；不可用清空 artifacts 或重寫 main 作回滾。

## 9. 整合測試與驗收矩陣

以下 `AC-LB-*` 是需求驗收 ID，不是已存在的 cargo target。實作時在 `tests/` 註冊真實 target、加入專案測試索引／CI；共享 `integration.md` 的 DB migration 和 compose 基礎。

| ID | 場景 | 必須斷言 |
|---|---|---|
| AC-LB-01 | 真 Libra 捕獲→上傳→CL evidence 查詢；混合版本／未知 schema | 三個身分（session、bundle、change）可追溯；同內容重送一份收據；不支援版本在寫入前拒絕 |
| AC-LB-02 | 同 UUID 同大小異內容；proxy／signed／multipart；併發 finalize；冒用 producer／v1 路徑 | 舊內容不可替換；digest 不符無 committed manifest；不同租戶及同租戶無權 producer 不互相探測；v1 不可觸及 evidence key／manifest |
| AC-LB-03 | 上傳中断、事件亂序、缺件、finalize／GC 競態、配額 | 有限重試與 missing 診斷；缺件不能 pass；有效 lease 不被 GC；背壓不產生半份 committed 資料 |
| AC-LB-04 | 同使用者同路徑兩 Agent；HTTP／SSH／Web 更新；舊 Git；legacy 回填 | 指定 change 分離；stale expected revision 拒絕；legacy 多候選拒絕且有選擇方法；新 Agent CL 不改變原本唯一 legacy CL 的成功推送；未解析 owner 不被冒名認領 |
| AC-LB-05 | rebase、人工補改、squash、被否決 patch、root 前移 | 舊 revision 及來源可查；指纹變動使證據／批准 stale，不可靜默繼承 |
| AC-LB-06 | 冒名上傳、越權 path、過期／撤銷委託、混合 Bot/User、Cedar off／shadow | payload 不能決定 principal；接收、finalize、落地重查；off／shadow＋evidence 啟用在 startup/reload 拒絕，不得接收並採信證據；未知 route/action 不可放行 |
| AC-LB-07 | 無 trace 權限但有 code 權限；摘要／搜尋／下載／快取；v1 旁路／未執行授權 | 派生面與 legacy URL 不洩漏 evidence；off／shadow 不能經旧端點讀／改 evidence；即時撤銷走 proxy；到期 signed URL 不能再用 |
| AC-LB-08 | 自述 passed、runner failed、錯 commit、偽 CI callback、政策更新、自審批 | 結果不互覆蓋；只接受正確 runner＋subject；required 拒絕 missing/stale/error/unverified；delegator 自審批預設拒絕、例外政策可追溯且不繞過 ACL 硬限制 |
| AC-LB-09 | 雙 server 併發 landing、root 前移、一般／內部／queue 入口 | 使用同 root writer；stale 不落地；授權／policy 在寫入時重查；無旁路及重複 commit |
| AC-LB-10 | path commit／root roll-up／多 commit squash／no-op 反向查詢 | 多對多 provenance 完整；no-op 無假 SHA；原始修改與最終樹可校驗 |
| AC-LB-11 | 投影清空重建、亂序 outbox、陳舊搜尋水位 | 重建結果一致；不重複通知；gate 不依赖落後索引；未知／缺失明示 |
| AC-LB-12 | DB 前後各 crash point、回應遺失、備份還原、migration 中断 | main 與 landing mapping 同成敗；收據冪等；deletion ledger 先於對外恢復生效 |
| AC-LB-13 | 脫敏失敗、保留到期、刪除、hold、共享 blob、舊 bundle 重播 | raw 不洩漏；活引用不被誤刪；被刪內容不復活；未落地證據失效則阻止 gate |
| AC-LB-14 | 不完整／錯 revision 依賴圖，跨服務修改 | 空圖不等於無影響；使用固定保守 checks；未知圖不能減少 required checks |
| AC-LB-15 | 外部 SCM webhook 重播／亂序、PR 新 head、權限撤銷 | 舊 check 不代表新 head；錯簽拒絕；缺 branch protection 時顯示 unenforced |

實作階段依 `AGENTS.md` 執行 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test --all`；涉及 `src/` 另跑 `cargo build` 與 `cargo build --tests`。不得用 ignore／軟跳過把未驗證的真客户端、雙 server 或外部整合報為通過。本文文件交付只驗證內容、連結及 review，不宣稱上述程式碼門禁已執行。

## 10. 風險與約束

| 風險 | 影響 | 緩解措施 |
|---|---|---|
| 證據存得住但内容不可信 | 誤把自述當成測試通過 | 分級來源、server digest、受信任 runner、required policy |
| queue／landing 雙重建設 | 根寫入競態及溯源分裂 | LB-06 強依赖既有 trunk-push root writer，统一事務 |
| 過度保守的 stale 策略 | 主幹活躍時重測與排隊成本上升 | 首版正確性優先，量測 stale 比例；後續以可證明的輸入閉包重用 |
| 捕獲 schema 與模型工具快速變動 | 靜默丟失事件、錯誤完整性聲明 | adapter／schema version、coverage、golden fixtures、unknown 保留 |
| raw／摘要外洩或刪除後復活 | 客戶失去資料控制 | raw opt-in、分層權限、deletion ledger、restore 演練 |
| 功能擴散到完整 Agent 平台 | 核心閉環遲遲無法交付 | 核心階段 1–4、可選階段 5 分開；固定 CI checks 可先落地 |

約束另列：不能改變 monorepo 既有公開 Git 規則；不能把 storage-only 部署強接 website；不能在新 gate 出錯時自動降級；不能把證據審查 PASS 宣稱為軟體正確性或法規認證。違反這些邊界會破壞既有部署或產品承諾，需先修訂對應契約並 review。

## 11. 多維評估

以下數字是需求設計評估，並非產品實測評分或對交付成熟度的背書。

| 維度 | 評估結論、當前不足與改進方向 |
|---|---|
| 合理性 | 9/10：直接連接捕獲與合併；仍需真實團隊驗證評審需求，避免只展示 transcript |
| 可行性 | 7/10：已有 artifacts／CL／授權底座；跨域事務與 root writer 尚需實作，分階段驗收 |
| 完整性 | 8/10：含同步、授權、遷移、刪除與回復；wire 細節仍待階段 0 凍結 |
| 安全性 | 7/10：明確 trust／raw／撤銷邊界；需以负向測試证明，不能只靠 schema |
| 功能正確性 | 8/10：精確版本綁定及 no-op 語義明確；需跨入口／多對多 lineage fixtures |
| 可靠性 | 7/10：有 outbox、冪等與 crash 要求；實際 crash／雙 server 測試尚未完成 |
| 相容性 | 8/10：保留 v1 OID、legacy Git 和 off 模式；新舊節點 fencing 必須補齊 |
| 可擴展性 | 7/10：有 staging、配額、分頁及可重建投影；容量／延遲目標須由試點基準決定 |
| 資料治理 | 7/10：有分層保留與 deletion ledger；hold／備份範圍需客戶政策配置及演練 |

## 12. 小結與預期收益

本重構把 mega2 的既有 artifacts、CL、授權與主幹寫入接成 Agent 變更的中央證據鏈。Libra 提供開發過程的來源資料，mega2 確保資料與特定候選版本、批准及 landing 正確關聯。實作先完成可信攝取、任務隔離與原生合併把關，再擴展歷史檢索及外部 SCM。根寫入、身分與資料治理沿用既有專題約束，避免形成互相競爭的事實源。

可觀測收益：評審人工時間、一次驗證通過率、stale 檢查攔截數、合併後返工、從 landed commit 還原來源的成功率與耗時。先以同類型／難度任務建立基準，再觀測導入後差異；不得把所有變化直接歸因於本系統。完成標準是團隊在真實變更中反覆使用這條流程，不是保存更多 token。
