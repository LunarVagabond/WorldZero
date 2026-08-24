//! Zone-link crossing detection (#45) — whether a movement segment
//! crosses one of the zone's declared `content::manifest::Link` edges,
//! meaning the mover is leaving this zone for `target_zone` rather than
//! just moving within it. `Zone::tick` checks this before running
//! ordinary in-bounds movement validation.

use content::manifest::Link;

use crate::spatial::Point;

/// The first link (in manifest declaration order) whose edge the
/// segment `from -> to` crosses, if any. `content::manifest::Link::edge`
/// is exactly two points (a line segment) per
/// docs/specs/Content_Manifest_Spec.md; a link with fewer than two edge
/// points (shouldn't happen — the manifest format requires it, but this
/// stays defensive rather than panicking) never matches.
pub fn crossed_link(links: &[Link], from: Point, to: Point) -> Option<&Link> {
    links.iter().find(|link| {
        let (Some(&a), Some(&b)) = (link.edge.first(), link.edge.get(1)) else {
            return false;
        };
        segments_intersect(from, to, a, b)
    })
}

/// Standard orientation-based segment intersection test — true if
/// segment `p1-p2` crosses segment `p3-p4`. Degenerate collinear/touching
/// cases resolve to "not crossing" (an orientation of exactly zero fails
/// the strict `>` sign-change comparisons below) — acceptable for v0's
/// "walked through a portal" case, not meant to handle a move that lands
/// exactly on the link's line without straddling it.
fn segments_intersect(p1: Point, p2: Point, p3: Point, p4: Point) -> bool {
    let d1 = orientation(p3, p4, p1);
    let d2 = orientation(p3, p4, p2);
    let d3 = orientation(p1, p2, p3);
    let d4 = orientation(p1, p2, p4);

    (d1 > 0.0) != (d2 > 0.0) && (d3 > 0.0) != (d4 > 0.0)
}

fn orientation(a: Point, b: Point, c: Point) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(target: &str, edge: [(f64, f64); 2]) -> Link {
        Link {
            target_zone: target.to_string(),
            edge: edge.to_vec(),
            bidirectional: true,
        }
    }

    #[test]
    fn a_move_crossing_the_edge_is_detected() {
        let links = vec![link("next-zone", [(10.0, 0.0), (10.0, 10.0)])];
        let crossed = crossed_link(&links, (9.0, 5.0), (11.0, 5.0));
        assert_eq!(crossed.map(|l| l.target_zone.as_str()), Some("next-zone"));
    }

    #[test]
    fn a_move_not_reaching_the_edge_is_not_detected() {
        let links = vec![link("next-zone", [(10.0, 0.0), (10.0, 10.0)])];
        assert!(crossed_link(&links, (5.0, 5.0), (9.0, 5.0)).is_none());
    }

    #[test]
    fn a_move_parallel_to_the_edge_and_never_reaching_it_is_not_detected() {
        let links = vec![link("next-zone", [(10.0, 0.0), (10.0, 10.0)])];
        assert!(crossed_link(&links, (5.0, 15.0), (5.0, 20.0)).is_none());
    }

    #[test]
    fn no_links_never_crosses() {
        assert!(crossed_link(&[], (0.0, 0.0), (100.0, 100.0)).is_none());
    }

    #[test]
    fn a_link_with_fewer_than_two_edge_points_never_matches() {
        // Defensive branch — `content::manifest::Link::edge` is validated
        // to be exactly 2 points before a manifest ever loads, but this
        // function stays defensive rather than assuming that always held
        // (doc comment above `crossed_link`). Directly exercises that
        // branch rather than trusting it's unreachable.
        let degenerate = Link {
            target_zone: "next-zone".to_string(),
            edge: vec![(10.0, 0.0)],
            bidirectional: true,
        };
        let links = vec![degenerate];
        assert!(crossed_link(&links, (9.0, 5.0), (11.0, 5.0)).is_none());
    }

    #[test]
    fn the_first_matching_link_in_declaration_order_wins() {
        let links = vec![
            link("zone-a", [(10.0, 0.0), (10.0, 10.0)]),
            link("zone-b", [(10.0, 0.0), (10.0, 10.0)]),
        ];
        let crossed = crossed_link(&links, (9.0, 5.0), (11.0, 5.0));
        assert_eq!(crossed.map(|l| l.target_zone.as_str()), Some("zone-a"));
    }
}
