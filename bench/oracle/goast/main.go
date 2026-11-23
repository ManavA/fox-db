// Command goast prints one JSON line per function declaration in the Go files named on the
// command line: {"file", "name", "recv", "line", "end", "exported"}. `line` is the line of the
// name; `end` is the last line of the body. Stdlib only, so the oracle stays independent of fox.
package main

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
)

type fn struct {
	File     string `json:"file"`
	Name     string `json:"name"`
	Recv     string `json:"recv"`
	Line     int    `json:"line"`
	End      int    `json:"end"`
	Exported bool   `json:"exported"`
}

func recvType(e ast.Expr) string {
	switch t := e.(type) {
	case *ast.StarExpr:
		return recvType(t.X)
	case *ast.IndexExpr:
		return recvType(t.X)
	case *ast.IndexListExpr:
		return recvType(t.X)
	case *ast.Ident:
		return t.Name
	}
	return ""
}

func main() {
	fset := token.NewFileSet()
	enc := json.NewEncoder(os.Stdout)
	for _, path := range os.Args[1:] {
		f, err := parser.ParseFile(fset, path, nil, parser.SkipObjectResolution)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			continue
		}
		for _, d := range f.Decls {
			fd, ok := d.(*ast.FuncDecl)
			if !ok || fd.Body == nil {
				continue
			}
			r := ""
			if fd.Recv != nil && len(fd.Recv.List) > 0 {
				r = recvType(fd.Recv.List[0].Type)
			}
			enc.Encode(fn{path, fd.Name.Name, r, fset.Position(fd.Name.Pos()).Line,
				fset.Position(fd.Body.Rbrace).Line, fd.Name.IsExported()})
		}
	}
}
