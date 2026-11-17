def nodes_that_fit(pod, nodes):
    """Drop every machine that cannot run the pod: not enough cpu or memory, or a taint the pod
    does not tolerate."""
    return [n for n in nodes if fits_resources(pod, n) and tolerates_taints(pod, n)]


def fits_resources(pod, node):
    """Compare the pod's requests with what the node has left."""
    return pod.cpu <= node.free_cpu and pod.memory <= node.free_memory


def tolerates_taints(pod, node):
    return all(t in pod.tolerations for t in node.taints)


def score_nodes(pod, nodes):
    """Rank the remaining machines, emptiest first."""
    return sorted(nodes, key=lambda n: -n.free_cpu)
