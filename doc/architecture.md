# Архитектурное решение: Unified MITM Proxy

## 1. Введение и цели

### Задача
Безопасная изоляция учётных данных (API-ключей Anthropic, токенов GitHub) от агентных виртуальных машин, при этом обеспечивая прозрачный доступ к API.

### Ключевые требования качества

| Приоритет | Атрибут | Описание |
|-----------|---------|----------|
| 1 | Безопасность | ВМ никогда не видит реальные токены |
| 2 | Прозрачность | Приложения внутри ВМ работают со стандартными URL |
| 3 | Поддержка стриминга | SSE/chunked-ответы от Anthropic API |
| 4 | Простота | Минимум компонентов, нет внешних зависимостей на хосте |

### Заинтересованные стороны

| Роль | Ожидание |
|------|----------|
| Оператор | Простой запуск и настройка |
| Агент (Claude/OpenCode) | Прозрачный доступ к API без модификаций |
| Безопасник | Учётные данные не утекают в ВМ |

## 2. Ограничения

- ВМ управляются через Lima/QEMU
- Хост ↔ ВМ связь через `host.lima.internal`
- На хосте только стандартная библиотека Python
- В ВМ допускается установка пакетов через pip

## 3. Контекст системы

```
┌─────────────────────────────────────────────────────────────────┐
│ Хост                                                            │
│                                                                 │
│  Токены ──► credential-proxy.py ──────► api.anthropic.com       │
│             (127.0.0.1:PORT)            github.com              │
│                  ▲                      api.github.com          │
│                  │ HTTP                                         │
│ ─ ─ ─ ─ ─- ─ ─ ─ │ -----─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ │
│ Lima VM          │                                              │
│                  │                                              │
│  Claude ──► mitmdump ────────────► pypi.org, npm и др.          │
│  git       (HTTPS_PROXY)          (passthrough, без MITM)       |
│  gh                                                             │
└─────────────────────────────────────────────────────────────────┘
```

## 4. Стратегия решения

**Было (3 прокси + URL-хаки):**
- `claude-vm-proxy.py` — прокси для Anthropic API
- `github-git-proxy.py` — прокси для git через `url.insteadOf`
- `github-mcp-proxy.py` — MCP-прокси для GitHub
- Подмена `ANTHROPIC_BASE_URL`, `git config url.*.insteadOf`

**Стало (2-уровневый MITM):**
- `mitmproxy-addon.py` — перехват HTTPS внутри ВМ
- `credential-proxy.py` — инъекция токенов на хосте

**Причина замены:** 3 прокси + URL-хаки — хрупкая архитектура. Приложения ожидают реальные URL, а подмена ломает валидацию, редиректы и конфигурацию. MITM-подход прозрачен для приложений.

## 5. Строительные блоки

### 5.1. credential-proxy.py (хост)

Единый HTTP-прокси, заменяющий три предыдущих.

**Конфигурация** (`CREDENTIAL_PROXY_RULES` env):
```json
[
  {"domain": "api.anthropic.com", "headers": {"Authorization": "Bearer sk-..."}},
  {"domain": "github.com", "path_prefix": "/org1/repo1", "headers": {"Authorization": "Basic <base64-token1>"}},
  {"domain": "github.com", "path_prefix": "/org2/repo2", "headers": {"Authorization": "Basic <base64-token2>"}},
  {"domain": "github.com", "headers": {"Authorization": "Basic <base64-fallback>"}},
  {"domain": "api.github.com", "path_prefix": "/repos/org1/repo1", "headers": {"Authorization": "token TOKEN1"}},
  {"domain": "api.github.com", "path_prefix": "/repos/org2/repo2", "headers": {"Authorization": "token TOKEN2"}},
  {"domain": "api.github.com", "headers": {"Authorization": "token FALLBACK"}}
]
```

Опциональное поле `path_prefix` позволяет разные токены для разных репозиториев.
Правила сортируются по длине префикса (longest match first), правила без префикса — fallback.

**Поток обработки запроса:**
1. Получить HTTP-запрос от mitmproxy
2. Проверить `X-Proxy-Token` (если `CREDENTIAL_PROXY_SECRET` задан) → 403 при несовпадении
3. Извлечь `X-Original-Host`, `X-Original-Port`, `X-Original-Scheme`
4. Сопоставить домен + путь с правилами (longest path_prefix first)
5. Перезаписать заголовки авторизации (удаляя существующие)
6. Убрать `Accept-Encoding`, `X-Proxy-Token` из upstream-запроса
7. Переслать на upstream по HTTPS
8. Вернуть ответ (с re-chunking для стриминга)

**Лимиты:** 32 МБ тело запроса, 300с таймаут upstream, 60с таймаут входящих.

### 5.2. mitmproxy-addon.py (ВМ)

Аддон для mitmdump (~30 строк). Перехватывает HTTPS-трафик для настроенных доменов.

**Принцип работы:**
- Фильтрация по `CREDENTIAL_PROXY_DOMAINS` (comma-separated) — только перечисленные домены перенаправляются на credential-proxy
- Остальной трафик (pypi, npm, Docker Hub) проходит через mitmproxy без изменений к реальным upstream
- Для перехваченных запросов: добавить `X-Original-*` и `X-Proxy-Token` заголовки, перенаправить на хост-прокси по HTTP

**Запуск:** через `systemd-run --user` для выживания после завершения SSH-сессии `limactl shell`.

### 5.3. Интеграция в claude-vm.sh

**Последовательность запуска:**
1. Получить токены (Anthropic из `CLAUDE_VM_PROXY_ACCESS_TOKEN`, GitHub App через device auth)
2. Собрать `CREDENTIAL_PROXY_RULES` JSON (пропускается, если нет токенов)
3. Запустить `credential-proxy.py` на хосте → получить порт (пропускается, если правила пусты)
4. Клонировать и загрузить ВМ
5. Записать фиктивные учётные данные Claude (только если `CLAUDE_VM_PROXY_ACCESS_TOKEN` задан)
6. Сгенерировать CA-сертификат mitmproxy, установить в доверенные
7. Установить прокси env vars через `/etc/profile.d/credential-proxy.sh`
8. Запустить mitmdump с аддоном через `systemd-run --user`
9. Настроить git: credential helper (placeholder-токен) + `url.insteadOf` (SSH → HTTPS)
10. Настроить gh CLI: `hosts.yml` + `config.yml` с `version: "1"` (предотвращает миграцию)
11. Запустить агента (без `ANTHROPIC_BASE_URL`)

**Получение токенов для подмодулей:**
Для каждого GitHub-подмодуля запрашивается scoped-токен через device auth flow. Если GitHub App установлен на репозитории и пользователь имеет доступ — токен выдаётся. Если нет — запрос завершается неудачей без прерывания запуска ВМ.

**Условный запуск:**
- `CLAUDE_VM_PROXY_ACCESS_TOKEN` не задан → фиктивные `.credentials.json` не создаются, Claude Code не запускается
- Нет ни одного токена (ни Anthropic, ни GitHub) → credential-proxy и mitmproxy не запускаются, ВМ работает без прокси

## 6. Решения об архитектуре

### ADR-1: mitmproxy вместо URL-подмены

| | URL-подмена (было) | MITM-прокси (стало) |
|---|---|---|
| Прозрачность | Приложения видят подменённые URL | Приложения видят реальные URL |
| Компонентов | 3 прокси + конфиг-хаки | 2 компонента |
| Новые домены | Добавить прокси + insteadOf | Добавить правило в JSON |
| Зависимости | Нет | mitmproxy в ВМ (~15 МБ) |

**Решение:** MITM-подход. Дополнительная зависимость оправдана радикальным упрощением.

### ADR-2: Per-repo авторизация через path_prefix

- `api.anthropic.com` → `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`
- `github.com/owner/repo` → `Authorization: Basic <base64(x-access-token:TOKEN_FOR_REPO)>` (git HTTP)
- `api.github.com/repos/owner/repo` → `Authorization: token <TOKEN_FOR_REPO>` (gh CLI, API)
- `github.com` (fallback) → первый доступный токен
- `api.github.com` (fallback) → первый доступный токен

Правила с `path_prefix` сортируются по длине (longest match first). Запросы к неизвестным репозиториям или к `/user`, `/search` и т.д. используют fallback.

**Причина:** GitHub App installation tokens привязаны к конкретным репозиториям. Проект с подмодулями из разных организаций требует разные токены.

### ADR-3: Фильтрация доменов в аддоне

Аддон проверяет `CREDENTIAL_PROXY_DOMAINS` (comma-separated) и перенаправляет только запросы к перечисленным доменам. Остальные проходят через mitmproxy без изменений к реальным upstream.

**Было:** `--ignore-hosts` с негативным lookahead regex. Не работало — mitmproxy туннелировал CONNECT-запросы без вызова `request()` хука.

**Стало:** Аддон сам фильтрует домены. mitmproxy перехватывает весь HTTPS (MITMs всё), но аддон перенаправляет на credential-proxy только настроенные домены.

**Причина:** Простота и надёжность. Regex + `--ignore-hosts` ломался из-за shell escaping и несовместимости между версиями mitmproxy.

### ADR-4: SSH → HTTPS через git insteadOf

ВМ настроена с `git config --global url."https://github.com/".insteadOf "git@github.com:"`.

**Причина:** Протокол SSH (порт 22) нельзя перехватить через HTTP-прокси. mitmproxy работает только с HTTP/HTTPS. Rewrite на HTTPS обеспечивает прохождение всего git-трафика через MITM-цепочку для инъекции токенов.

### ADR-5: Отказ от retry-логики

Прокси не повторяет запросы при ошибках upstream.

**Причина:** Клиенты (Claude SDK, git) имеют свою логику повторов. Двойные retry создают каскадные проблемы.

### ADR-6: Per-instance секрет для изоляции ВМ

**Угроза:** На одном хосте несколько ВМ с разными наборами репозиториев. ВМ-1 может обратиться к credential-proxy ВМ-2 через `host.lima.internal:PORT` и получить токены чужих репозиториев.

**Решение:** При каждом запуске генерируется 256-бит случайный секрет (`secrets.token_hex(32)`). Секрет передаётся:
- credential-proxy через `CREDENTIAL_PROXY_SECRET` env var
- mitmproxy через `CREDENTIAL_PROXY_SECRET` env var

Аддон добавляет `X-Proxy-Token` к каждому запросу. Прокси проверяет его перед инъекцией — несовпадение → 403. Заголовок удаляется перед пересылкой upstream.

**Альтернатива:** Привязка к IP/порту — ненадёжна (ВМ делят сетевой стек хоста через Lima). Секрет проще и надёжнее.

## 7. Риски и технический долг

| Риск | Вероятность | Влияние | Митигация |
|------|-------------|---------|-----------|
| mitmproxy обновление ломает аддон | Низкая | Средняя | Аддон использует стабильный API (request hook) |
| Новый домен требует MITM | Средняя | Низкая | Добавить правило в JSON + домен в `CREDENTIAL_PROXY_DOMAINS` |
| CA-сертификат просрочится | Низкая | Высокая | Генерируется при каждом запуске ВМ |
| Upstream меняет формат авторизации | Низкая | Высокая | Правила конфигурируемы через JSON |
| mitmproxy не стартует в ВМ | Средняя | Высокая | `systemd-run --user` + проверка готовности + journalctl диагностика |

## 8. Тестирование

42 автоматических теста (`test_credential_proxy.py`):

- **TestCredentialProxy** (11) — инъекция заголовков, HTTP-методы, пути, дубликаты
- **TestCredentialProxyErrors** (4) — 400/413/502 ошибки
- **TestCredentialProxySecret** (4) — проверка секрета, 403 при несовпадении, strip перед upstream
- **TestCredentialProxyUnmatched** (1) — домены без правил
- **TestCredentialProxyStreaming** (1) — chunked-ответы
- **TestCredentialProxyPathMatching** (4) — per-repo маршрутизация по path_prefix
- **TestCredentialProxyPathMatching** (4) — per-repo маршрутизация по path_prefix
- **TestCredentialProxyStartup** (3) — запуск, некорректный JSON, SIGTERM
- **TestBuildCredentialRules** (9) — генерация per-repo правил из токенов
- **TestMitmproxyAddon** (4) — перезапись flow, секрет

Запуск: `python3 -m unittest test_credential_proxy -v`
