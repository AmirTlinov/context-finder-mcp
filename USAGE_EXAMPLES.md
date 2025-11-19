# Context Finder - Примеры использования

## 🚀 Быстрый старт

### 1. Установка

```bash
cd context-finder
cargo build --release

# Установить глобально
cargo install --path crates/cli

# Или использовать напрямую
alias context-finder='./target/release/context-finder'
```

### 2. Индексация проекта

```bash
# Индексировать текущий проект
context-finder index .

# Индексировать конкретную директорию
context-finder index /path/to/project
```

**Вывод:**
```json
{
  "files": 247,
  "chunks": 1893,
  "total_lines": 45832,
  "time_ms": 8347,
  "languages": {
    "rust": 158,
    "python": 52,
    "javascript": 37
  },
  "errors": []
}
```

### 3. Поиск кода

```bash
# Базовый поиск
context-finder search "async error handling" --limit 5

# С указанием проекта
context-finder search "database query" -p /path/to/project -l 10

# Verbose режим для отладки
context-finder -v search "authentication"
```

**Вывод:**
```json
{
  "query": "async error handling",
  "results": [
    {
      "file": "src/api/middleware/error.rs",
      "start_line": 15,
      "end_line": 42,
      "symbol": "handle_error",
      "type": "function",
      "score": 0.92,
      "content": "pub async fn handle_error(err: ApiError) -> Response {\n    match err {\n        ApiError::NotFound => ...\n    }\n}",
      "context": [
        "use axum::response::Response",
        "use crate::types::ApiError"
      ]
    }
  ]
}
```

### 4. Получить контекст вокруг строки

```bash
# Контекст вокруг конкретной строки (для навигации ИИ)
context-finder get-context src/main.rs 42 --window 20

# С указанием проекта
context-finder get-context src/lib.rs 100 -p /path/to/project
```

**Вывод:**
```json
{
  "file": "src/main.rs",
  "line": 42,
  "symbol": "process_request",
  "type": "function",
  "parent": "RequestHandler",
  "imports": [
    "use tokio::sync::mpsc",
    "use serde::Deserialize"
  ],
  "content": "async fn process_request(req: Request) -> Result<Response> {\n    // processing logic\n}",
  "window": {
    "before": "// Previous 20 lines...",
    "after": "// Next 20 lines..."
  }
}
```

### 5. Список символов в файле

```bash
# Получить все символы (функции, классы, структуры)
context-finder list-symbols src/api/handler.rs

# С проектом
context-finder list-symbols src/models/user.rs -p /path/to/project
```

**Вывод:**
```json
{
  "file": "src/api/handler.rs",
  "symbols": [
    {
      "name": "ApiHandler",
      "type": "struct",
      "parent": null,
      "line": 10
    },
    {
      "name": "new",
      "type": "method",
      "parent": "ApiHandler",
      "line": 15
    },
    {
      "name": "handle_request",
      "type": "method",
      "parent": "ApiHandler",
      "line": 23
    }
  ]
}
```

## 🤖 Использование с ИИ-моделями

### Через Bash tools (Claude, GPT, etc.)

```python
# Python example для использования с LangChain/Instructor
import subprocess
import json

def search_code(query: str, limit: int = 10) -> dict:
    """Search code semantically"""
    result = subprocess.run(
        ["context-finder", "search", query, "-l", str(limit)],
        capture_output=True,
        text=True
    )
    return json.loads(result.stdout)

def get_context(file: str, line: int) -> dict:
    """Get context around specific line"""
    result = subprocess.run(
        ["context-finder", "get-context", file, str(line)],
        capture_output=True,
        text=True
    )
    return json.loads(result.stdout)

# Использование
results = search_code("error handling in async functions")
for r in results["results"]:
    print(f"{r['file']}:{r['start_line']} - {r['symbol']} (score: {r['score']:.2f})")

# Получить контекст
context = get_context("src/main.rs", 42)
print(f"Symbol: {context['symbol']} ({context['type']})")
print(f"Content:\n{context['content']}")
```

### TypeScript/Node.js example

```typescript
import { exec } from 'child_process';
import { promisify } from 'util';

const execAsync = promisify(exec);

async function searchCode(query: string, limit: number = 10) {
  const { stdout } = await execAsync(
    `context-finder search "${query}" -l ${limit}`
  );
  return JSON.parse(stdout);
}

async function listSymbols(file: string) {
  const { stdout } = await execAsync(
    `context-finder list-symbols "${file}"`
  );
  return JSON.parse(stdout);
}

// Использование
const results = await searchCode('async error handling');
console.log(`Found ${results.results.length} results`);

results.results.forEach((r: any) => {
  console.log(`${r.file}:${r.start_line} - ${r.symbol} (${r.score.toFixed(2)})`);
});
```

## 📊 Реальные примеры запросов

### Поиск по концепции (semantic)

```bash
# Находит код по смыслу, а не по точному совпадению
context-finder search "error handling"
# Найдет: try/catch блоки, Result<T>, Error types, etc.

context-finder search "authentication logic"
# Найдет: login functions, JWT validation, session management

context-finder search "database queries"
# Найдет: SQL, ORM queries, repository patterns

context-finder search "async operations"
# Найдет: async/await, futures, promises

context-finder search "data validation"
# Найдет: validators, schema checks, sanitization
```

### Поиск по имени (fuzzy)

```bash
# Находит по частичному совпадению имени
context-finder search "getUserById"
context-finder search "handleError"
context-finder search "ApiHandler"

# Работает с опечатками
context-finder search "proces"  # найдет "process"
context-finder search "usre"    # найдет "user"
```

### Навигация по коду

```bash
# 1. Найти где реализована функция
context-finder search "process_payment" | jq '.results[0] | {file, line: .start_line}'

# 2. Получить контекст этой функции
context-finder get-context $(jq -r '.results[0].file' results.json) \
                           $(jq -r '.results[0].start_line' results.json)

# 3. Посмотреть все символы в файле
context-finder list-symbols src/payment/processor.rs
```

## 🎯 Advanced: Интеграция в workflow

### Pre-commit hook для поиска TODO

```bash
#!/bin/bash
# .git/hooks/pre-commit

# Найти все TODO в изменённых файлах
for file in $(git diff --cached --name-only); do
    if [[ $file =~ \.(rs|py|js|ts)$ ]]; then
        symbols=$(context-finder list-symbols "$file" 2>/dev/null)
        if echo "$symbols" | grep -q "TODO"; then
            echo "Warning: TODO found in $file"
        fi
    fi
done
```

### CI/CD: Проверка покрытия документации

```bash
#!/bin/bash
# scripts/check-docs.sh

# Найти все публичные функции без документации
results=$(context-finder search "pub fn" -l 1000)

undocumented=0
echo "$results" | jq -r '.results[] | select(.content | contains("///") | not) | .file + ":" + (.start_line | tostring) + " - " + .symbol' > undocumented.txt

if [ -s undocumented.txt ]; then
    echo "⚠️  Undocumented public functions found:"
    cat undocumented.txt
    exit 1
fi
```

### VS Code task integration

```json
{
  "version": "2.0.0",
  "tasks": [
    {
      "label": "Index Project",
      "type": "shell",
      "command": "context-finder index .",
      "problemMatcher": [],
      "group": "build"
    },
    {
      "label": "Search Code",
      "type": "shell",
      "command": "context-finder search '${input:searchQuery}' -l 20",
      "problemMatcher": []
    }
  ],
  "inputs": [
    {
      "id": "searchQuery",
      "type": "promptString",
      "description": "Enter search query"
    }
  ]
}
```

## 📈 Performance tips

### Оптимизация поиска

```bash
# Semantic-only (быстрее, но менее точно для имен)
RUST_LOG=debug context-finder search "query"  # посмотреть timing

# Для больших проектов: переиндексация с кешированием
context-finder index . && context-finder search "query"

# Limit результатов для скорости
context-finder search "query" -l 5  # вместо 10
```

### Мониторинг производительности

```bash
# Benchmark поиска
time context-finder search "error handling" -l 10

# Проверить размер индекса
du -h .context-finder/index.json

# Статистика индексации
context-finder index . | jq '{files, chunks, time_ms, languages}'
```

---

**Готово!** Context Finder работает как нативный CLI инструмент с полной JSON интеграцией для ИИ-моделей 🚀
