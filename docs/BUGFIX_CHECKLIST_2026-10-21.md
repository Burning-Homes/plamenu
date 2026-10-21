# Plamenu: девять исправлений от 2026-10-21

Запрос: независимо подтвердить и исправить девять багов, проверять выборочно,
один раз пройти общие проверки, затем развернуть результат на staging.

Рабочая ветка: `feat/ap09-native-video`. Существовавшие до задачи изменения
`docs/api/index.md` и `docs/ECOSYSTEM_FEATURE_PROPOSALS.md` сохранены отдельно.
Все коммиты задачи создаются с DCO и подписью; подписи проверены локально.

## План и результат

- [x] 1. Убрать дубли при подписке на автора и группу.
- [x] 2. Исправить private mention/reply/like/reaction notifications, push URL
  и синхронизацию прочтения с private inbox.
- [x] 3. Уменьшить ALT, убрать clipping, реализовать повторное закрытие и
  сохранить выделение текста, включая медленный double-click.
- [x] 4. Устранить первый кадр скачка меню, сохранить границы viewport и
  место под мобильной навигацией, включая no-JS.
- [x] 5. Добавить якорь опубликованного ответа и проверить длинный тред.
- [x] 6. Устранить скачки длинного композера и сохранить видимость курсора.
- [x] 7. Располагать неполный последний ряд вложений слева направо.
- [x] 8. Убрать дубли inline media, сохранить остальные вложения, подключить
  общий lightbox/player и native fallback.
- [x] 9. Показывать Unicode-домены, включая self-reply и подписи действий.
- [x] Выполнить полный Rust/browser прогон, разобрать связанные регрессии,
  повторить затронутые проверки.
- [ ] Полный межсерверный E2E: заблокирован ключами тестового Mastodon;
  выполнены доступные адресные проверки с другими peers.
- [x] Deploy to staging и smoke-проверка версии, inline media и IDN.

## Подтверждения и исправления

| Пункт | Причина и решение | Проверка |
| --- | --- | --- |
| 1 | SQL подавлял Group Announce только для Video. Обе сортировки home теперь скрывают wrapper, когда оригинал самостоятельно проходит фильтры ленты. Учитываются свои посты, язык, ответы, exclusive lists, provenance и пагинация. | Regression падал до исправления; DB и remote-group tests PASS. Живой тест двух Plamenu: подписка на группу и автора даёт один оригинал у читателя и автора; reply/edit/delete работают. |
| 2 | Native inbox и unread bell исключали direct. Tagless direct replies не всегда создавали mention notification. Исправлены адресаты, inbox/read state; push содержит конкретный URL с якорем, service worker проверяет origin и сохраняет вкладки с черновиками. | 65 серверных notification/direct/web tests PASS; push tests PASS; 6 service-worker tests PASS. Живые private conversations с GoToSocial/Plamenu2 PASS. Browser inbox/read-state в семи конфигурациях PASS. |
| 3 | Hover/focus tooltip обрезался overflow и не имел toggle. Теперь native top-layer popover и кнопка 32×28 px. JS закрывает текст по простому клику; drag, long press, выделение и double-click сохраняются. Окно single-click — 600 ms. | 13 ALT browser cases PASS. Отдельно воспроизведено закрытие при 400 ms между кликами со старым таймером 300 ms; новый вариант сохраняет выделение во всех трёх движках. |
| 4 | JS placePopup запускался после появления details. Размещение перенесено в CSS anchor positioning с flip и ограничением высоты; для остальных движков есть ограниченная viewport панель. | 7 browser cases PASS: координаты всех первых кадров, три положения по высоте, mobile и no-JS. |
| 5 | 303 Location после reply не содержал fragment. Ответы получают `#post-ID`; HTTP test helpers читают pathname отдельно от fragment. | Серверные reply tests PASS; живой long-thread тест inline/dedicated composer, включая no-JS, PASS. |
| 6 | JS сбрасывал textarea height на каждом input; воспроизведён скачок страницы на 1344 px. Теперь ограниченный CSS field-sizing, нативная прокрутка textarea и VisualViewport limit. | 6 browser cases PASS: 90 строк, начало/середина/конец, покадровый scroll, уменьшенный viewport клавиатуры. |
| 7 | Первое из трёх вложений занимало два grid-row. Исключение удалено. | 7 browser cases PASS для 3 и 5 вложений, включая no-JS. |
| 8 | В примере WordPress inline `gcp_status-scaled.png` и attachment `gcp_status-1024x650.png` имели разные media IDs. Добавлены ограниченные одинаковым origin/path/extension/query WordPress aliases. Inline images получают общий lightbox; proxied inline audio/video — общий player/native controls. Остальные attachments сохраняются. | 11 целевых Rust tests и 7 browser cases PASS с реальным декодированием видео. В общем прогоне найден stack overflow первой regex compilation в gateway; regex initialization перенесена в AppState, gateway + inline 6/6 PASS. |
| 9 | reply_context передавал сырой acct в Fluent. Исправлены self/other reply, selection labels, domain actions и music fallback. Canonical IDs/URLs сохранены. | Armenian IDN unit tests, relationships и privileged menu tests PASS. |

Исходные публичные примеры для smoke:

- Inline media: https://staging3.plamenu.com/@antranigv@xn--y9aai3au2bc2f.xn--y9a3aq/117444945448468480#post-117444945448468480
- IDN: https://staging3.plamenu.com/@norayr@xn--69aa8bzb.xn--y9a3aq
- Публичные self-reply ID для IDN: `117444897308737536`,
  `117444897306902528`, `117444897104658432`.

## Коммиты по пунктам

1. `f8d484a` — Group deduplication.
2. `ea4403e` — Private notifications/read state/push.
3. `72f5383` — ALT popover и выделение.
4. `7e74090` — CSS menu placement.
5. `4aaf88d` — Reply fragment.
6. `1604ecc` — Composer sizing.
7. `77b66d9` — Attachment row order.
8. `33c7c3b` — Inline media.
9. `fb43796` — Unicode reply/account labels.

Дополнения: `3e57a87` — Unicode audit и корректная проверка managed-actor
notification renderer; `a09390b` — ранняя regex initialization и test URL parsing.
Начальный план: `506dc8c`. Финальные browser/E2E ожидания и медленный double-click:
`06f1d879`; итоговые доказательства и public-tree fixture: `0fcbd86a`.

## Общие проверки и ограничения

**Финальная адресная browser-проверка: 61/61 PASS, 3.9 минуты.**
Включены grid/menu/ALT/composer/reply-anchor/inline media, inbox/read-state и
native video upload/playback. Chromium, Firefox, WebKit, desktop/mobile и no-JS.

- Полный Rust: 3555 tests, 3514 passed, 41 failed, 2 skipped, без fail-fast.
  Только эти 41 тест перепроверен на исходном `5a8603a7`: 37 также падают до
  изменений. Это discovery, schema/SQL marker checks и прежние UI expectations.
  Из четырёх отличий URL parsing test и gateway stack overflow исправлены;
  reconciliation timeout и HLS counter прошли при отдельной проверке.
- Полный browser: 1128 сценариев, 976 passed, 104 failed, 48 skipped, 1.8 часа.
  14 failures — устаревшие ожидания inbox/ALT размера; исправлены и вошли в
  финальный успешный прогон. Остальные результаты не объявляются PASS:
  Music import/Create album/player assertions, отсутствие внешних
  `A11Y_IMPORT_SOURCE_URL`/`A11Y_SOURCE_SYNC_URL`, video-time color contrast,
  предел WebKit fullPage screenshot 32767 px. Они не исправлялись в этой задаче.
  Отдельная прежняя проблема обрезания video timer описана в
  `docs/ACCESSIBILITY_JOURNEY.md`, Deferred timer investigation — A59.
- Node: 61/61 PASS. Python unittest: 145, OK с 3 optional skips.
  Benchmark budget unit tests: 18/18 PASS. Tracked syntax/WCAG matrix PASS.
- ShellCheck, закреплённый Ruff 0.16.4, форматирование, DCO/seed-version и
  Gitleaks PASS. Public-tree на чистом commit snapshot PASS.
  Server Clippy всех targets с online SQLx завершён (exit 0),
  новых предупреждений нет. Строгий общий Clippy остаётся с замечаниями в
  неизменённых discovery/search/two_factor; offline all-target также встречает
  два прежних rate_limit query без SQLx metadata.
- `cargo deny`: 11 advisories для существующего Cargo.lock; bans/licenses/sources
  PASS. Зависимости не изменялись этой задачей.
- Полный E2E собрал 402 теста, но остановлен после подтверждения системного
  сбоя Mastodon: /api/v2/search HTTP 500 и ActiveRecord Decryption в Sidekiq.
  Частичный итог: 38 failed / 11 passed за 27 минут. Три доступных набора ENV
  не расшифровывают сохранённый RSA Keypair; Rails CONFIG_MATCH проверен.
  Данные/ключи не сбрасывались, криптографическая проверка не обходилась.
- Адресные E2E: 6 сценариев с GtS/Plamenu2 PASS (private/reply/favourite/reaction).
  Обновлённый group author+group тест отдельно PASS, 46.92 s. Inline Article
  E2E также зависит от Mastodon для signed AP GET и остаётся заблокирован;
  assertion не удалён/не ослаблен.

## Staging

Развёрнута сборка **332**, исходный commit `0fcbd86a69e0`.
API сообщает `4.7.1 (compatible; Plamenu 0.7.0-staging.332+sha.0fcbd86a69e0.dirty)`.
Суффикс dirty связан с сохранёнными пользовательскими изменениями документации;
изменения кода задачи закоммичены. Deploy завершился успешно, health HTTP 200.

В исходном публичном WordPress-посте одна inline-картинка, ноль дублирующих
галерей и корректная ссылка на полноразмерное изображение. Self-reply
`117444897308737536` показывает `Replying to @norayr@թութ.հայ`.
Мобильный Chromium на staging открывает изображение в lightbox,
декодирует его и закрывает по Escape. Без JavaScript нативная ссылка открывает
полноразмерное изображение, редирект на сохранённый AVIF и декодирование PASS.

## Локальные доказательства

Все подробные логи находятся в игнорируемой `.dev/`:

- `bugs-20261021-final-browser-targeted.log` — 61/61.
- `bugs-20261021-full-browser-fixed-candidate.log` и
  `bugs-20261021-full-browser-report.json` — полный browser; screenshots/traces
  сохранены в `bugs-20261021-full-browser-test-results/`.
- `bugs-20261021-full-rust.log`, `bugs-20261021-baseline-rust-failures.log`,
  `bugs-20261021-rust-differential-followup.log`,
  `bugs-20261021-current-cache-restored.log` — Rust и baseline comparison.
- `bugs-20261021-full-e2e.log/xml`, `bugs-20261021-full-e2e-matrix.json`,
  `bugs-20261021-healthy-peers.log/xml`, `bugs-20261021-group-e2e-final.log`.
- `bugs-20261021-server-clippy-online.log`, `bugs-20261021-quick-rest.log`,
  `bugs-20261021-quick-tools.log` — статические проверки и baseline limitations.
- `bugs-20261021-deploy-staging.log`, `bugs-20261021-staging-smoke.json`,
  `bugs-20261021-staging-browser.log` (JS lightbox),
  `bugs-20261021-staging-browser-nojs.log` — deployment и публичные live-проверки.

Временная baseline worktree удалена. Её сборка разделяла Cargo target с основной
копией: после обнаружения смешанного dependency-кэша все четыре текущие crate
библиотеки принудительно пересобраны; gateway/inline 6/6 PASS. В дальнейшем
разным worktree нужны отдельные target directories.

Все тестовые процессы завершены. Deployment и публичные smoke-проверки выполнены.

Финальный follow-up: `06f1d879`, подпись проверена. При проверке чистого
commit snapshot public-tree обнаружены два запрещённых домена-заглушки в
migration-recovery browser fixture; заменены на зарезервированный
`example.test`, сохраняя проверку неизвестного адресата. Тестовые серверы и
38 поднятых задачей peer-контейнеров остановлены; исходные Caddy/PeerTube,
dev Postgres/Mailpit и посторонние сервисы сохранены.
