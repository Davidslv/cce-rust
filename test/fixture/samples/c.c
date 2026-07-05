#include <stdlib.h>

struct Node {
    int value;
};

int sum_node(struct Node *n) {
    return n->value;
}
