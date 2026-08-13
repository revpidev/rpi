flowchart TD
  A[Start] --> B{Is it ready?}
  B -->|Yes| C[Ship it]
  B -->|No| D[Fix bugs]
  D --> B
  subgraph sub[Sub process]
    E[Step 1] --> F[Step 2]
  end
  C --> E
