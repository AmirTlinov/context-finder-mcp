# Быстрый старт с Context Finder

## 🎯 Что это?

Context Finder — это инструмент для **мгновенной навигации по кодовым базам**, разработанный специально для интеграции с ИИ-моделями (Claude, GPT, и др.). Он понимает структуру кода и находит нужные фрагменты за миллисекунды.

## 📦 Установка

### Вариант 1: Cargo (рекомендуется)

```bash
cargo install context-finder-cli
```

### Вариант 2: Из исходников

```bash
git clone https://github.com/yourusername/context-finder
cd context-finder
cargo build --release
sudo cp target/release/context-finder /usr/local/bin/
```

### Вариант 3: Binary releases

Скачайте готовый бинарник для вашей платформы из [GitHub Releases](https://github.com/yourusername/context-finder/releases).

## 🚀 Первые шаги

### 1. Индексация проекта

```bash
# Перейдите в папку проекта
cd ~/my-project

# Запустите индексацию
context-finder index .

# Вывод:
# 📦 Scanning project...
# ✓ Found 247 files
# 🔍 Parsing code...
# ✓ Created 1,893 chunks
# 🧮 Computing embeddings...
# ✓ Built vector index
# ✅ Indexing completed in 8.3s
```

### 2. Поиск по проекту

```bash
# Простой поиск
context-finder search "error handling"

# Вывод:
# 🔍 Search results for "error handling":
#
# 1. src/api/middleware/error.rs:15-42 (score: 0.92)
#    Function: handle_error
#    Error handling middleware for HTTP requests
#
# 2. src/utils/result.rs:8-25 (score: 0.87)
#    Struct: ApiResult
#    Custom result type with error context
#
# 3. tests/integration/error_test.rs:30-55 (score: 0.81)
#    Function: test_error_response
#    Test error handling in API responses
```

### 3. Интерактивный режим (TUI)

```bash
context-finder interactive
```

Откроется интерактивный интерфейс:
- `Ctrl+F`: Поиск
- `↑/↓`: Навигация по результатам
- `Enter`: Открыть файл в редакторе
- `q`: Выход

## 📚 Примеры использования

### Поиск функций по описанию

```bash
context-finder search "parse JSON from string"
context-finder search "validate user input"
context-finder search "async database query"
```

### Поиск по имени символа

```bash
context-finder search "getUserById"
context-finder search "class AuthMiddleware"
context-finder search "interface IRepository"
```

### Поиск с фильтрами

```bash
# Только Rust файлы
context-finder search "async fn" --lang rust

# Только в определённой директории
context-finder search "test" --path src/tests/

# Ограничить количество результатов
context-finder search "handler" --limit 5
```

### Экспорт результатов

```bash
# В JSON
context-finder search "api endpoint" --format json > results.json

# В Markdown
context-finder search "database" --format markdown > report.md

# В CSV
context-finder search "util" --format csv > utils.csv
```

## 🔧 Конфигурация

Создайте файл `.context-finder.toml` в корне проекта:

```toml
# Стратегия chunking
[chunking]
strategy = "semantic"  # semantic, line_count, token_aware
target_tokens = 512
max_tokens = 1024
include_imports = true
include_documentation = true

# Настройки поиска
[search]
fusion_strategy = "reciprocal_rank"  # reciprocal_rank, weighted, max_score
semantic_weight = 0.7
fuzzy_weight = 0.3
candidate_pool_size = 50
cache_enabled = true

# Игнорируемые паттерны (дополнительно к .gitignore)
[ignore]
patterns = [
    "node_modules/",
    "target/",
    "*.lock",
    "dist/",
]

# Поддерживаемые языки
[languages]
supported = ["rust", "python", "javascript", "typescript"]
```

## 🤖 Интеграция с ИИ (MCP Server)

### Для Claude Code

Добавьте в `~/.claude/config.json`:

```json
{
  "mcpServers": {
    "context-finder": {
      "command": "context-finder",
      "args": ["mcp", "--project", "/path/to/your/project"]
    }
  }
}
```

Теперь Claude может использовать команды:
- `search_codebase("query")` — поиск по проекту
- `get_chunk("id")` — получить конкретный фрагмент
- `get_context("file", line)` — получить контекст вокруг строки

### Для других ИИ

Context Finder совместим с любыми ИИ через MCP протокол:

```bash
# Запуск MCP сервера
context-finder mcp --port 8080 --project .
```

## 📊 Статистика проекта

```bash
context-finder stats

# Вывод:
# 📊 Project Statistics
#
# Files:           247
# Total lines:     45,832
# Code chunks:     1,893
# Avg tokens/chunk: 428
#
# Languages:
#   Rust:          158 files (64%)
#   Python:        52 files (21%)
#   JavaScript:    37 files (15%)
#
# Index size:      12.4 MB
# Last indexed:    2 minutes ago
```

## 🔄 Обновление индекса

```bash
# Полная переиндексация
context-finder reindex

# Инкрементальное обновление (только изменённые файлы)
context-finder update

# Автоматическое обновление при изменениях (watch mode)
context-finder watch
```

## 🎓 Продвинутые возможности

### 1. Batch поиск

Создайте файл `queries.txt`:
```
error handling
async functions
database queries
authentication
```

Запустите:
```bash
context-finder batch queries.txt --output results/
```

### 2. Code navigation

```bash
# Найти все ссылки на функцию
context-finder references "getUserById"

# Найти определение символа
context-finder definition "ApiError"

# Показать контекст вокруг строки
context-finder context src/main.rs:42
```

### 3. Clustering похожего кода

```bash
# Найти похожие фрагменты
context-finder similar src/api/users.rs:15-30

# Найти дублирующийся код
context-finder duplicates --threshold 0.9
```

## 🐛 Решение проблем

### Индексация медленная

```bash
# Используйте быструю стратегию
context-finder index . --strategy line_count

# Исключите большие файлы
echo "large_data/" >> .gitignore
```

### Нет результатов поиска

```bash
# Проверьте индекс
context-finder stats

# Переиндексируйте
context-finder reindex

# Используйте более широкий запрос
context-finder search "error" --fuzzy-threshold 0.5
```

### Высокое потребление памяти

```bash
# Уменьшите размер кеша
context-finder config set cache_size 50

# Используйте меньший embedding dimension
context-finder config set embedding_dim 256
```

## 📖 Дополнительные ресурсы

- [Архитектура](ARCHITECTURE.md) — детальное описание внутреннего устройства
- [API Documentation](https://docs.rs/context-finder) — для использования как библиотеки
- [GitHub Issues](https://github.com/yourusername/context-finder/issues) — баг-репорты и feature requests

## 💬 Поддержка

- Discord: https://discord.gg/context-finder
- GitHub Discussions: https://github.com/yourusername/context-finder/discussions
- Email: support@context-finder.dev

---

**Готовы начать?** Запустите `context-finder index .` в вашем проекте! 🚀
