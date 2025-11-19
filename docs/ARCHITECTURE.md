# Архитектура Context Finder

## 📐 Общая структура

Context Finder построен по модульному принципу с четким разделением ответственности между компонентами:

```
context-finder/
├── crates/
│   ├── code-chunker/      # Семантическое разбиение кода
│   ├── vector-store/      # Векторное хранилище
│   ├── indexer/           # Индексация проектов
│   ├── retrieval/         # Гибридный поиск
│   ├── cli/               # CLI интерфейс
│   └── mcp-server/        # MCP Server для ИИ
├── docs/                  # Документация
├── examples/              # Примеры использования
└── Cargo.toml            # Workspace configuration
```

## 🔄 Data Flow

### 1. Индексация проекта

```
   File System
        │
        ├─► Git Repository (.gitignore aware)
        │
        ▼
   ┌─────────────────┐
   │  File Scanner   │ ──► Parallel file reading
   └────────┬────────┘     (tokio::spawn tasks)
            │
            ▼
   ┌──────────────────┐
   │  Code Chunker    │
   │  (Tree-sitter)   │
   ├──────────────────┤
   │ • Parse AST      │
   │ • Extract funcs  │
   │ • Add context    │
   │ • Compute meta   │
   └────────┬─────────┘
            │
            ├─────────────────┬─────────────────┐
            ▼                 ▼                 ▼
   ┌──────────────┐  ┌──────────────┐  ┌─────────────┐
   │ Vector Store │  │ Fuzzy Index  │  │  Metadata   │
   │              │  │              │  │   Store     │
   │ Embeddings   │  │ Path index   │  │ Symbols DB  │
   │ HNSW build   │  │ Content idx  │  │ Relations   │
   └──────────────┘  └──────────────┘  └─────────────┘
            │                 │                 │
            └─────────────────┴─────────────────┘
                              │
                              ▼
                      ┌──────────────┐
                      │ Persist to   │
                      │ Disk (.idx/) │
                      └──────────────┘
```

### 2. Поиск по проекту

```
   User Query: "async error handling"
        │
        ▼
   ┌─────────────────────┐
   │  Query Processor    │
   │  • Tokenize         │
   │  • Normalize        │
   │  • Extract keywords │
   └─────────┬───────────┘
             │
             ├──────────────────────┬──────────────────┐
             ▼                      ▼                  ▼
    ┌────────────────┐    ┌──────────────────┐  ┌────────────┐
    │  Fuzzy Search  │    │ Semantic Search  │  │  Metadata  │
    │   (nucleo)     │    │  (embeddings)    │  │   Filter   │
    ├────────────────┤    ├──────────────────┤  ├────────────┤
    │ • Path match   │    │ • Query vector   │  │ • Lang     │
    │ • Content fuzz │    │ • HNSW search    │  │ • Type     │
    │ • Rank by sim  │    │ • Cosine sim     │  │ • Scope    │
    └───────┬────────┘    └────────┬─────────┘  └──────┬─────┘
            │                      │                    │
            └──────────┬───────────┴────────────────────┘
                       │
                       ▼
              ┌─────────────────┐
              │  Fusion Engine  │
              │   (RRF/Hybrid)  │
              ├─────────────────┤
              │ RRF formula:    │
              │ score = Σ 1/    │
              │   (k + rank_i)  │
              │                 │
              │ Weights:        │
              │ • Fuzzy: 0.3    │
              │ • Semantic: 0.7 │
              └────────┬────────┘
                       │
                       ▼
              ┌─────────────────┐
              │   Reranker      │
              │  (Contextual)   │
              ├─────────────────┤
              │ • Boost by meta │
              │ • Recent edits  │
              │ • Importance    │
              └────────┬────────┘
                       │
                       ▼
              ┌─────────────────┐
              │ Final Results   │
              │ [ {chunk, score,│
              │    metadata} ]  │
              └─────────────────┘
```

## 🧩 Компоненты подробно

### Code Chunker

**Ответственность:** Разбиение кода на семантически значимые фрагменты

**Технологии:**
- Tree-sitter для AST parsing
- Language detection по расширениям
- Metadata extraction (symbols, types, imports)

**Стратегии chunking:**
```rust
enum ChunkingStrategy {
    Semantic,        // По границам функций/классов (AST)
    LineCount,       // Фиксированное число строк
    TokenAware,      # По токенам с учетом синтаксиса
    Hierarchical,    // Иерархический (parent + children)
}
```

**Output:**
```rust
CodeChunk {
    file_path: String,
    start_line: usize,
    end_line: usize,
    content: String,
    metadata: ChunkMetadata {
        language: "rust",
        chunk_type: Function,
        symbol_name: "process_data",
        parent_scope: Some("DataProcessor"),
        imports: vec!["std::io", "serde::Deserialize"],
        estimated_tokens: 245,
    }
}
```

### Vector Store

**Ответственность:** Векторизация и индексация для семантического поиска

**Pipeline:**
```
Content → Embedding Model → Vector[384] → HNSW Index → Disk
```

**Технологии:**
- **FastEmbed**: Быстрые CPU embeddings (всего ~50MB памяти)
- **HNSW**: Hierarchical Navigable Small World graphs для ANN
- **Persistence**: JSON для metadata + binary для vectors

**Performance:**
- Embedding: 5-15ms per chunk (batch: 2-5ms per chunk)
- Index build: O(n log n) для n chunks
- Search: O(log n) с ~50-100 hops
- Memory: ~1KB per chunk + embeddings

### Retrieval System

**Ответственность:** Гибридный поиск с fusion и reranking

**Multi-stage pipeline:**

**Stage 1: Candidate Retrieval**
```
Fuzzy (Top 50) + Semantic (Top 50) → Pool of 100 candidates
```

**Stage 2: Fusion (RRF)**
```python
def reciprocal_rank_fusion(rankings, k=60):
    scores = defaultdict(float)
    for rank_list in rankings:
        for rank, item in enumerate(rank_list):
            scores[item] += 1 / (k + rank + 1)
    return sorted(scores.items(), key=lambda x: -x[1])
```

**Stage 3: Reranking**
```
• Boost recent edits (git blame)
• Boost by importance (references count)
• Boost by type (function > variable)
• Contextual similarity (cross-encoder optional)
```

**Strategies:**
```rust
enum FusionStrategy {
    ReciprocalRank,   // RRF (default)
    WeightedScore,    // Linear combination
    MaxScore,         // Best score wins
    SemanticOnly,     // Pure embeddings
    FuzzyOnly,        // Pure lexical
}
```

### Indexer

**Ответственность:** Сканирование и индексация проектов

**Features:**
- Параллельная обработка (rayon/tokio)
- .gitignore aware (через `ignore` crate)
- Инкрементальные обновления (inotify/FSEvents)
- Progress tracking (indicatif)

**Index structure:**
```
.context-finder/
├── chunks.json         # Metadata
├── vectors.bin         # HNSW index
├── fuzzy.idx          # Fuzzy index
└── stats.json         # Statistics
```

### CLI

**Ответственность:** Пользовательский интерфейс

**Commands:**
```bash
context-finder index <path>              # Index project
context-finder search <query>            # Search
context-finder reindex                   # Rebuild index
context-finder stats                     # Show statistics
context-finder interactive               # TUI mode
context-finder export --format json      # Export results
```

**TUI Features:**
- Live search with debouncing
- File preview with syntax highlighting
- Keyboard navigation
- Multi-select for bulk operations

### MCP Server

**Ответственность:** Интеграция с ИИ-моделями через MCP

**Protocol:**
```json
{
  "jsonrpc": "2.0",
  "method": "tools/list",
  "result": {
    "tools": [
      {
        "name": "search_codebase",
        "description": "Search for code semantically",
        "inputSchema": { ... }
      },
      {
        "name": "get_chunk",
        "description": "Get specific chunk by ID",
        "inputSchema": { ... }
      }
    ]
  }
}
```

**Endpoints:**
- `search_codebase(query, limit)` → SearchResults
- `get_chunk(id)` → CodeChunk
- `get_context(file, line)` → Context
- `list_symbols(file)` → Symbols[]

## 🎛️ Конфигурация

### Performance Presets

```rust
// Для максимальной скорости
ChunkerConfig::for_speed()
RetrievalConfig::fast()

// Для максимальной точности
ChunkerConfig::for_llm_context()
RetrievalConfig::accurate()

// Для embeddings (баланс)
ChunkerConfig::for_embeddings()
RetrievalConfig::default()
```

### Tunable Parameters

| Parameter | Default | Range | Impact |
|-----------|---------|-------|--------|
| `target_chunk_tokens` | 512 | 128-2048 | Chunk size |
| `candidate_pool_size` | 50 | 10-200 | Recall vs speed |
| `semantic_weight` | 0.7 | 0.0-1.0 | Semantic vs fuzzy |
| `rrf_k` | 60 | 10-100 | Fusion sensitivity |
| `cache_size` | 100 | 0-1000 | Memory vs speed |

## 🔬 Алгоритмы

### Reciprocal Rank Fusion (RRF)

```
Вход: Rankings R1, R2, ..., Rm (по n элементов каждый)
Параметр: k (обычно 60)

Для каждого элемента d:
    score(d) = Σ(i=1 to m) 1 / (k + rank_i(d))

где rank_i(d) — позиция элемента d в рейтинге R_i
(если d отсутствует в R_i, то rank_i(d) = ∞)

Выход: Элементы, отсортированные по убыванию score(d)
```

**Преимущества:**
- Робастность к outliers
- Не требует нормализации скоров
- Хорошо работает с разнородными источниками

### HNSW (Hierarchical Navigable Small World)

```
Построение индекса:
1. Создать слои графа (Level 0, 1, 2, ...)
2. Для каждого вектора v:
   - Выбрать layer_level случайно (exponential decay)
   - Вставить в графы уровней 0..layer_level
   - Связать с M ближайшими соседями на каждом уровне

Поиск:
1. Начать с entry point на верхнем уровне
2. Жадно двигаться к ближайшим соседям
3. При достижении локального минимума — спуститься ниже
4. На Level 0 — собрать ef ближайших
5. Вернуть top-k из ef
```

**Параметры:**
- M = 16 (connections per node)
- ef_construction = 200 (build quality)
- ef_search = 50 (search quality)

## 💡 Дизайн-решения

### Почему гибридный поиск?

| Scenario | Fuzzy | Semantic | Hybrid |
|----------|-------|----------|--------|
| "getUserById" (exact name) | ✅ Perfect | ❌ Partial | ✅ Perfect |
| "error handling pattern" | ❌ Poor | ✅ Good | ✅ Excellent |
| "auth middleware" (concept) | 🟡 OK | ✅ Great | ✅ Great |
| Typos: "usre" → "user" | ✅ Good | ❌ Bad | ✅ Good |

**Вывод:** Hybrid даёт лучшее из обоих миров

### Почему Tree-sitter?

| Alternative | Pros | Cons |
|-------------|------|------|
| regex | Fast, simple | Breaks on edge cases |
| LSP | Accurate, rich | Slow, heavy, language-specific |
| Tree-sitter | Fast, accurate, multi-lang | Needs grammars |

**Выбор:** Tree-sitter для баланса скорости и точности

### Почему RRF?

| Method | Pros | Cons |
|--------|------|------|
| Weighted sum | Simple | Needs normalization |
| Max score | Fast | Ignores other signals |
| RRF | Robust, no normalization | Slight overhead |

**Выбор:** RRF как золотой стандарт в IR research

## 📈 Масштабирование

### Большие проекты (>1M LOC)

**Стратегии:**
1. **Sharding**: Разбить индекс по модулям
2. **Incremental**: Обновлять только изменённые файлы
3. **Lazy loading**: Подгружать vectors по требованию
4. **Compression**: Quantize embeddings (384d → 192d)

### Распределённая индексация

```
Master Node
    ├─► Worker 1: src/module_a/
    ├─► Worker 2: src/module_b/
    └─► Worker 3: tests/

Results → Merge → Final Index
```

## 🛡️ Ограничения и trade-offs

| Аспект | Ограничение | Workaround |
|--------|-------------|------------|
| Memory | ~1KB per chunk | Shard large projects |
| Embedding speed | CPU-bound | Batch operations, GPU option |
| Language support | Tree-sitter only | Fallback to regex |
| Real-time updates | Debounce 500ms | Acceptable for dev |
| Cold start | Index build ~10s per 100K LOC | Cache, incremental |

---

**Context Finder** — архитектура для flagship-level производительности 🏗️
