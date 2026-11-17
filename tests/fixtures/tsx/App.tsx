function Button(props: { label: string }) {
  return <button>{props.label}</button>;
}

export function App() {
  return <Button label={title()} />;
}

function title(): string {
  return "hi";
}
