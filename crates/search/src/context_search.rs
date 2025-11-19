use crate::error::Result;
use crate::hybrid::HybridSearch;
use context_code_chunker::CodeChunk;
use context_graph::{
    AssembledContext, AssemblyStrategy, CodeGraph, ContextAssembler, GraphBuilder, GraphLanguage,
};
use context_vector_store::SearchResult;
use std::collections::HashSet;

/// Context-aware search with automatic related code assembly
///
/// This is the flagship feature for AI agents - automatic context gathering
pub struct ContextSearch {
    hybrid: HybridSearch,
    assembler: Option<ContextAssembler>,
}

/// Enriched search result with automatically assembled context
#[derive(Debug, Clone)]
pub struct EnrichedResult {
    /// Primary search result
    pub primary: SearchResult,

    /// Related chunks (automatically gathered from graph)
    pub related: Vec<RelatedContext>,

    /// Total context lines (for token estimation)
    pub total_lines: usize,

    /// Context assembly strategy used
    pub strategy: AssemblyStrategy,
}

#[derive(Debug, Clone)]
pub struct RelatedContext {
    pub chunk: CodeChunk,
    pub relationship_path: Vec<String>,
    pub distance: usize,
    pub relevance_score: f32,
}

impl ContextSearch {
    /// Create new context-aware search (without graph initially)
    pub async fn new(hybrid: HybridSearch) -> Result<Self> {
        Ok(Self {
            hybrid,
            assembler: None,
        })
    }

    /// Build code graph for context assembly
    ///
    /// This should be called after indexing to enable context-aware search
    pub fn build_graph(&mut self, language: GraphLanguage) -> Result<()> {
        log::info!("Building code graph for context-aware search");

        let chunks: Vec<CodeChunk> = self.hybrid.chunks().to_vec();

        let mut builder = GraphBuilder::new(language)?;
        let graph = builder.build(&chunks)?;

        log::info!(
            "Code graph built: {} nodes, {} edges",
            graph.node_count(),
            graph.edge_count()
        );

        // Create assembler with the graph (doesn't need to clone)
        let assembler = ContextAssembler::new(graph);
        let graph_stats = (assembler.get_stats().total_nodes, assembler.get_stats().total_edges);

        // Store assembler (which owns the graph)
        self.assembler = Some(assembler);

        // We'll get graph info from assembler when needed
        log::info!("Graph stats: {} nodes, {} edges", graph_stats.0, graph_stats.1);

        Ok(())
    }

    /// Search with automatic context assembly (flagship feature)
    ///
    /// Returns search results with related code automatically gathered
    pub async fn search_with_context(
        &mut self,
        query: &str,
        limit: usize,
        strategy: AssemblyStrategy,
    ) -> Result<Vec<EnrichedResult>> {
        // Perform hybrid search
        let results = self.hybrid.search(query, limit).await?;

        // If no graph, return non-enriched results
        let assembler = match &self.assembler {
            Some(a) => a,
            None => {
                log::warn!("No graph available, returning non-enriched results");
                return Ok(results
                    .into_iter()
                    .map(|r| EnrichedResult {
                        total_lines: r.chunk.line_count(),
                        primary: r,
                        related: vec![],
                        strategy: strategy.clone(),
                    })
                    .collect());
            }
        };

        // Enrich each result with context
        let mut enriched = Vec::new();
        for result in results {
            let chunk_id = &result.id;

            // Assemble context for this chunk
            match assembler.assemble_for_chunk(chunk_id, strategy.clone()) {
                Ok(assembled) => {
                    let related = assembled
                        .related_chunks
                        .into_iter()
                        .map(|rc| RelatedContext {
                            chunk: rc.chunk,
                            relationship_path: rc
                                .relationship
                                .iter()
                                .map(|r| format!("{:?}", r))
                                .collect(),
                            distance: rc.distance,
                            relevance_score: rc.relevance_score,
                        })
                        .collect();

                    enriched.push(EnrichedResult {
                        total_lines: assembled.total_lines,
                        primary: result,
                        related,
                        strategy: strategy.clone(),
                    });
                }
                Err(e) => {
                    log::warn!("Failed to assemble context for {}: {}", chunk_id, e);
                    // Fallback to non-enriched result
                    enriched.push(EnrichedResult {
                        total_lines: result.chunk.line_count(),
                        primary: result,
                        related: vec![],
                        strategy: strategy.clone(),
                    });
                }
            }
        }

        log::info!(
            "Enriched {} results with context (avg {} related chunks per result)",
            enriched.len(),
            enriched.iter().map(|e| e.related.len()).sum::<usize>() / enriched.len().max(1)
        );

        Ok(enriched)
    }

    /// Batch search with context assembly
    pub async fn search_batch_with_context(
        &mut self,
        queries: &[&str],
        limit: usize,
        strategy: AssemblyStrategy,
    ) -> Result<Vec<Vec<EnrichedResult>>> {
        let mut all_enriched = Vec::new();

        for query in queries {
            let enriched = self.search_with_context(query, limit, strategy.clone()).await?;
            all_enriched.push(enriched);
        }

        Ok(all_enriched)
    }

    /// Get graph statistics
    pub fn graph_stats(&self) -> Option<(usize, usize)> {
        self.assembler.as_ref().map(|a| {
            let stats = a.get_stats();
            (stats.total_nodes, stats.total_edges)
        })
    }

    /// Check if graph is available
    pub fn has_graph(&self) -> bool {
        self.assembler.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_search_creation() {
        // Basic test that context search can be created
        // Real tests require FastEmbed model
    }
}
