/// Cosine similarity between two equal-length vectors.
/// Returns a value in [-1, 1]. Returns 0.0 for zero vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "embedding dimension mismatch");
    if a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
    }
}

/// Return indices of the top-k most similar vectors to `query` from `candidates`.
#[allow(dead_code)]
pub fn top_k_similar(query: &[f32], candidates: &[Vec<f32>], k: usize) -> Vec<(usize, f32)> {
    let mut scored: Vec<(usize, f32)> = candidates
        .iter()
        .enumerate()
        .map(|(i, emb)| (i, cosine_similarity(query, emb)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_vectors() {
        let v = vec![1.0f32, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_orthogonal_vectors() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-6);
    }

    #[test]
    fn test_opposite_vectors() {
        let a = vec![1.0f32, 0.0];
        let b = vec![-1.0f32, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_zero_vector_returns_zero() {
        let a = vec![0.0f32, 0.0, 0.0];
        let b = vec![1.0f32, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_top_k_similar() {
        let query = vec![1.0f32, 0.0, 0.0];
        let candidates = vec![
            vec![1.0f32, 0.0, 0.0], // identical
            vec![0.0f32, 1.0, 0.0], // orthogonal
            vec![0.8f32, 0.6, 0.0], // close
        ];
        let results = top_k_similar(&query, &candidates, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 0); // identical is best
        assert_eq!(results[1].0, 2); // close is second
    }

    #[test]
    fn test_clamped_to_minus_one_one() {
        // Due to floating point, ensure result is clamped
        let v = vec![1.0f32; 100];
        let score = cosine_similarity(&v, &v);
        assert!(score >= -1.0 && score <= 1.0);
    }
}
