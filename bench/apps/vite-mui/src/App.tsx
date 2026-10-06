import { useState } from 'react'
import AppBar from '@mui/material/AppBar'
import Toolbar from '@mui/material/Toolbar'
import Typography from '@mui/material/Typography'
import Container from '@mui/material/Container'
import Card from '@mui/material/Card'
import CardContent from '@mui/material/CardContent'
import CardActions from '@mui/material/CardActions'
import Button from '@mui/material/Button'
import TextField from '@mui/material/TextField'
import Stack from '@mui/material/Stack'
import Chip from '@mui/material/Chip'
import { ThemeProvider, createTheme } from '@mui/material/styles'
import CssBaseline from '@mui/material/CssBaseline'
import AddIcon from '@mui/icons-material/Add'
import DeleteIcon from '@mui/icons-material/Delete'

const theme = createTheme({ palette: { primary: { main: '#5b4bdb' } } })

function App() {
  const [items, setItems] = useState<string[]>(['Acropolis', 'Rolldown'])
  const [value, setValue] = useState('')
  return (
    <ThemeProvider theme={theme}>
      <CssBaseline />
      <AppBar position="static">
        <Toolbar>
          <Typography variant="h6">vite-mui</Typography>
        </Toolbar>
      </AppBar>
      <Container sx={{ mt: 4 }}>
        <Card>
          <CardContent>
            <Typography variant="h5" gutterBottom>Items</Typography>
            <Stack direction="row" spacing={1} sx={{ mb: 2 }}>
              {items.map((i) => (
                <Chip key={i} label={i} onDelete={() => setItems(items.filter((x) => x !== i))} deleteIcon={<DeleteIcon />} />
              ))}
            </Stack>
            <TextField label="New item" value={value} onChange={(e) => setValue(e.target.value)} size="small" />
          </CardContent>
          <CardActions>
            <Button variant="contained" startIcon={<AddIcon />} onClick={() => { if (value) { setItems([...items, value]); setValue('') } }}>
              Add
            </Button>
          </CardActions>
        </Card>
      </Container>
    </ThemeProvider>
  )
}

export default App
